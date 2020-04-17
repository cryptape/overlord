#![allow(unused_imports)]
#![allow(unused_variables)]

use std::collections::HashSet;
use std::error::Error;
use std::marker::PhantomData;
use std::sync::Arc;
use std::task::{Context as TaskCx, Poll};
use std::time::{Duration, Instant};
use std::{future::Future, pin::Pin};

use creep::Context;
use derive_more::Display;
use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::{select, StreamExt, TryFutureExt};
use futures::{FutureExt, SinkExt};
use futures_timer::Delay;
use log::{error, warn};

use crate::auth::{AuthCell, AuthFixedConfig, AuthManage};
use crate::cabinet::{Cabinet, Capsule};
use crate::error::ErrorInfo;
use crate::exec::ExecRequest;
use crate::state::Step::Propose;
use crate::state::{ProposePrepare, Stage, StateInfo, Step};
use crate::timeout::{TimeoutEvent, TimeoutInfo};
use crate::types::{
    ChokeQC, FetchedFullBlock, PreCommitQC, PreVoteQC, Proposal, SignedChoke, SignedPreCommit,
    SignedPreVote, SignedProposal, UpdateFrom,
};
use crate::{
    Adapter, Address, Blk, CommonHex, ExecResult, Hash, Height, HeightRange, OverlordConfig,
    OverlordError, OverlordMsg, OverlordResult, PriKeyHex, Proof, Round, St, TimeConfig, Wal,
    INIT_ROUND,
};

const POWER_CAP: u32 = 5;
const TIME_DIVISOR: u64 = 10;

const HEIGHT_WINDOW: Height = 5;
const ROUND_WINDOW: Round = 5;

pub type WrappedOverlordMsg<B> = (Context, OverlordMsg<B>);

/// State Machine Replica
pub struct SMR<A: Adapter<B, S>, B: Blk, S: St> {
    state:   StateInfo<B>,
    prepare: ProposePrepare<S>,

    adapter: Arc<A>,
    wal:     Wal,
    cabinet: Cabinet<B>,
    auth:    AuthManage<A, B, S>,
    agent:   EventAgent<A, B, S>,

    phantom_s: PhantomData<S>,
}

impl<A, B, S> SMR<A, B, S>
where
    A: Adapter<B, S>,
    B: Blk,
    S: St,
{
    pub async fn new(
        auth_fixed_config: AuthFixedConfig,
        adapter: &Arc<A>,
        from_net: UnboundedReceiver<(Context, OverlordMsg<B>)>,
        from_exec: UnboundedReceiver<ExecResult<S>>,
        to_exec: UnboundedSender<ExecRequest>,
        wal_path: &str,
    ) -> Self {
        let wal = Wal::new(wal_path);
        let rst = StateInfo::<B>::from_wal(&wal);
        let state = if let Err(e) = rst {
            warn!("Load wal failed! Try to recover state by the adapter, which face security risk if majority auth nodes lost their wal file at the same time");
            recover_state_by_adapter(adapter).await
        } else {
            rst.unwrap()
        };

        let height = state.stage.height;

        let prepare = recover_propose_prepare_and_config(adapter, height).await;
        let last_exec_result = prepare
            .exec_results
            .get(&prepare.exec_height)
            .expect("Unreachable! Cannot get last exec result");
        let auth_config = last_exec_result.consensus_config.auth_config.clone();
        let time_config = last_exec_result.consensus_config.time_config.clone();
        let last_config = if height > 0 {
            Some(
                get_exec_result(adapter, state.stage.height - 1)
                    .await
                    .unwrap()
                    .unwrap()
                    .consensus_config,
            )
        } else {
            None
        };

        let current_auth = AuthCell::new(auth_config, &auth_fixed_config.address);
        let last_auth: Option<AuthCell<B>> =
            last_config.map(|config| AuthCell::new(config.auth_config, &auth_fixed_config.address));

        SMR {
            wal,
            state,
            prepare,
            adapter: Arc::<A>::clone(adapter),
            cabinet: Cabinet::default(),
            auth: AuthManage::new(auth_fixed_config, current_auth, last_auth),
            agent: EventAgent::new(adapter, time_config, from_net, from_exec, to_exec),
            phantom_s: PhantomData,
        }
    }

    pub async fn run(mut self) {
        loop {
            select! {
                opt = self.agent.from_net.next() => {
                    if let Err(e) = self.handle_msg(opt.expect("Net Channel is down! It's meaningless to continue running")).await {
                        // self.adapter.handle_error()
                        error!("{}", e);
                    }
                }
                opt = self.agent.from_exec.next() => {
                    self.handle_exec_result(opt.expect("Exec Channel is down! It's meaningless to continue running"));
                }
                opt = self.agent.from_fetch.next() => {
                    if let Err(e) = self.handle_fetch(opt.expect("Fetch Channel is down! It's meaningless to continue running")).await {
                        // self.adapter.handle_error()
                        error!("{}", e);
                    }
                }
                opt = self.agent.from_timeout.next() => {
                    if let Err(e) = self.handle_timeout(opt.expect("Timeout Channel is down! It's meaningless to continue running")).await {
                        // self.adapter.handle_error()
                        error!("{}", e);
                    }
                }
            }
        }
    }

    async fn handle_msg(&mut self, wrapped_msg: WrappedOverlordMsg<B>) -> OverlordResult<()> {
        let (context, msg) = wrapped_msg;

        match msg {
            OverlordMsg::SignedProposal(signed_proposal) => {
                self.handle_signed_proposal(signed_proposal).await?;
            }
            OverlordMsg::SignedPreVote(signed_pre_vote) => {
                self.handle_signed_pre_vote(signed_pre_vote).await?;
            }
            OverlordMsg::SignedPreCommit(signed_pre_commit) => {
                self.handle_signed_pre_commit(signed_pre_commit).await?;
            }
            OverlordMsg::SignedChoke(signed_choke) => {
                self.handle_signed_choke(signed_choke).await?;
            }
            OverlordMsg::PreVoteQC(pre_vote_qc) => {
                self.handle_pre_vote_qc(pre_vote_qc).await?;
            }
            OverlordMsg::PreCommitQC(pre_commit_qc) => {
                self.handle_pre_commit_qc(pre_commit_qc).await?;
            }
            _ => {
                // ToDo: synchronization
            }
        }

        Ok(())
    }

    fn handle_exec_result(&mut self, exec_result: ExecResult<S>) {
        self.prepare.handle_exec_result(exec_result);
    }

    async fn handle_fetch(
        &mut self,
        fetch_result: OverlordResult<FetchedFullBlock>,
    ) -> OverlordResult<()> {
        let fetch = self.agent.handle_fetch(fetch_result)?;
        if fetch.height < self.state.stage.height {
            return Err(OverlordError::debug_old());
        }
        self.cabinet.insert_full_block(fetch.clone());
        self.wal.save_full_block(&fetch)?;
        // Todo: check if hash is waiting to process in PreVote Step or PreCommit Step

        Ok(())
    }

    async fn handle_timeout(&mut self, timeout_event: TimeoutEvent) -> OverlordResult<()> {
        match timeout_event {
            TimeoutEvent::ProposeTimeout(stage) => {}
            TimeoutEvent::PreVoteTimeout(stage) => {}
            TimeoutEvent::PreCommitTimeout(stage) => {}
            TimeoutEvent::BrakeTimeout(stage) => {}
            TimeoutEvent::NextHeightTimeout(height) => {}
        }
        Ok(())
    }

    async fn handle_signed_proposal(&mut self, sp: SignedProposal<B>) -> OverlordResult<()> {
        let msg_h = sp.proposal.height;
        let msg_r = sp.proposal.round;

        self.filter_msg(msg_h, msg_r, &sp.clone().into())?;
        // only msg of current height will go down
        self.check_proposal(&sp.proposal)?;
        self.auth.verify_signed_proposal(&sp)?;
        self.cabinet.insert(msg_h, msg_r, sp.clone().into())?;

        self.check_block(&sp.proposal.block).await?;
        self.agent.request_full_block(sp.proposal.block.clone());

        if sp.proposal.lock.is_none() && msg_r > self.state.stage.round {
            return Err(OverlordError::debug_high());
        }

        self.state.handle_signed_proposal(&sp)?;
        self.agent.set_timeout(self.state.stage.clone());
        self.state.save_wal(&self.wal)?;

        self.auth.can_i_vote()?;
        let vote = self.auth.sign_pre_vote(sp.proposal.as_vote())?;
        self.agent.transmit(sp.proposal.proposer, vote.into()).await
    }

    async fn handle_signed_pre_vote(&mut self, sv: SignedPreVote) -> OverlordResult<()> {
        let msg_h = sv.vote.height;
        let msg_r = sv.vote.round;

        self.filter_msg(msg_h, msg_r, &sv.clone().into())?;
        self.auth.verify_signed_pre_vote(&sv)?;
        if let Some(sum_w) = self.cabinet.insert(msg_h, msg_r, sv.clone().into())? {
            if self.auth.current_auth.beyond_majority(sum_w.cum_weight) {
                let votes = self
                    .cabinet
                    .get_signed_pre_votes_by_hash(
                        msg_h,
                        sum_w.round,
                        &sum_w
                            .block_hash
                            .expect("Unreachable! Lost the vote_hash while beyond majority"),
                    )
                    .expect("Unreachable! Lost signed_pre_votes while beyond majority");
                let pre_vote_qc = self.auth.aggregate_pre_votes(votes)?;
                self.agent.broadcast(pre_vote_qc.clone().into()).await?;
                self.handle_pre_vote_qc(pre_vote_qc).await?;
            }
        }
        Ok(())
    }

    async fn handle_signed_pre_commit(&mut self, sv: SignedPreCommit) -> OverlordResult<()> {
        let msg_h = sv.vote.height;
        let msg_r = sv.vote.round;

        self.filter_msg(msg_h, msg_r, &sv.clone().into())?;
        self.auth.verify_signed_pre_commit(&sv)?;
        if let Some(sum_w) = self.cabinet.insert(msg_h, msg_r, sv.clone().into())? {
            if self.auth.current_auth.beyond_majority(sum_w.cum_weight) {
                let votes = self
                    .cabinet
                    .get_signed_pre_commits_by_hash(
                        msg_h,
                        sum_w.round,
                        &sum_w
                            .block_hash
                            .expect("Unreachable! Lost the vote_hash while beyond majority"),
                    )
                    .expect("Unreachable! Lost signed_pre_votes while beyond majority");
                let pre_commit_qc = self.auth.aggregate_pre_commits(votes)?;
                self.agent.broadcast(pre_commit_qc.clone().into()).await?;
                self.handle_pre_commit_qc(pre_commit_qc).await?;
            }
        }
        Ok(())
    }

    async fn handle_signed_choke(&mut self, sc: SignedChoke) -> OverlordResult<()> {
        let msg_h = sc.choke.height;
        let msg_r = sc.choke.round;

        self.filter_msg(msg_h, msg_r, &sc.clone().into())?;
        self.auth.verify_signed_choke(&sc)?;
        if let Some(sum_w) = self.cabinet.insert(msg_h, msg_r, sc.clone().into())? {
            if self.auth.current_auth.beyond_majority(sum_w.cum_weight) {
                let votes = self
                    .cabinet
                    .get_signed_chokes(msg_h, sum_w.round)
                    .expect("Unreachable! Lost signed_chokes while beyond majority");
                let choke_qc = self.auth.aggregate_chokes(votes)?;
                self.handle_choke_qc(choke_qc).await?;
            } else {
                match sc.from {
                    UpdateFrom::PreVoteQC(qc) => self.handle_pre_vote_qc(qc).await?,
                    UpdateFrom::PreCommitQC(qc) => self.handle_pre_commit_qc(qc).await?,
                    UpdateFrom::ChokeQC(qc) => self.handle_choke_qc(qc).await?,
                }
            }
        }
        Ok(())
    }

    async fn handle_pre_vote_qc(&mut self, qc: PreVoteQC) -> OverlordResult<()> {
        let msg_h = qc.vote.height;
        let msg_r = qc.vote.round;

        self.filter_msg(msg_h, msg_r, &qc.clone().into())?;
        self.auth.verify_pre_vote_qc(&qc)?;
        self.cabinet.insert(msg_h, msg_r, qc.clone().into())?;
        if self
            .cabinet
            .get_full_block(msg_h, &qc.vote.block_hash)
            .is_some()
        {
            let block = self
                .cabinet
                .get_block(msg_h, &qc.vote.block_hash)
                .expect("Unreachable! Lost a block which full block exist");
            self.state.handle_pre_vote_qc(&qc, block.clone())?;
            self.agent.set_timeout(self.state.stage.clone());
            self.state.save_wal(&self.wal)?;

            self.auth.can_i_vote()?;
            let vote = self.auth.sign_pre_commit(qc.vote.clone())?;
            let leader = self.auth.get_leader(msg_h, msg_r);
            self.agent.transmit(leader, vote.into()).await?;
        }
        Ok(())
    }

    async fn handle_pre_commit_qc(&mut self, qc: PreCommitQC) -> OverlordResult<()> {
        let msg_h = qc.vote.height;
        let msg_r = qc.vote.round;

        self.filter_msg(msg_h, msg_r, &qc.clone().into())?;
        self.auth.verify_pre_commit_qc(&qc)?;
        self.cabinet.insert(msg_h, msg_r, qc.clone().into())?;
        if self
            .cabinet
            .get_full_block(msg_h, &qc.vote.block_hash)
            .is_some()
        {
            let block = self
                .cabinet
                .get_block(msg_h, &qc.vote.block_hash)
                .expect("Unreachable! Lost a block which full block exist");
            self.state.handle_pre_commit_qc(&qc, block.clone())?;
            self.state.save_wal(&self.wal)?;
            self.handle_commit().await?;
        }
        Ok(())
    }

    async fn handle_choke_qc(&mut self, qc: ChokeQC) -> OverlordResult<()> {
        let msg_h = qc.choke.height;
        let msg_r = qc.choke.round;

        self.filter_msg(msg_h, msg_r, &qc.clone().into())?;
        self.auth.verify_choke_qc(&qc)?;
        self.state.handle_choke_qc(&qc)?;
        self.state.save_wal(&self.wal)?;
        self.new_round().await
    }

    async fn handle_commit(&mut self) -> OverlordResult<()> {
        let proof = self
            .state
            .pre_commit_qc
            .as_ref()
            .expect("Unreachable! Lost pre_commit_qc when commit");
        let commit_hash = proof.vote.block_hash.clone();
        let height = self.state.stage.height;

        let full_block = self
            .cabinet
            .get_full_block(height, &commit_hash)
            .expect("Unreachable! Lost full block when commit");
        let request = ExecRequest::new(height, full_block.clone(), proof.clone());
        self.agent.save_and_exec_block(request);

        let commit_exec_h = self
            .state
            .block
            .as_ref()
            .expect("Unreachable! Lost commit block when commit")
            .get_exec_height();
        let next_height = height + 1;
        let commit_exec_result =
            self.prepare
                .handle_commit(commit_hash, proof.clone(), commit_exec_h, next_height);
        self.auth
            .handle_commit(commit_exec_result.consensus_config.auth_config);
        self.cabinet.handle_commit(next_height, &self.auth);

        // if self is leader, should not wait for interval timeout. This is different from previous
        // design.
        if !self.auth.am_i_leader(next_height, INIT_ROUND)
            && self.agent.set_timeout(self.state.stage.clone())
        {
            return Ok(());
        }
        self.next_height(commit_exec_result.consensus_config.time_config)
            .await
    }

    async fn next_height(&mut self, time_config: TimeConfig) -> OverlordResult<()> {
        self.state.next_height();
        self.state.save_wal(&self.wal)?;
        self.agent.next_height(time_config);
        self.new_round().await
    }

    async fn new_round(&mut self) -> OverlordResult<()> {
        // if leader send proposal else search proposal, last set time
        let h = self.state.stage.height;
        let r = self.state.stage.round;

        self.agent.set_timeout(self.state.stage.clone());

        if self.auth.am_i_leader(h, r) {
            let signed_proposal = self.create_signed_proposal().await?;
            self.agent.broadcast(signed_proposal.into()).await?;
        } else if let Some(signed_proposal) = self.cabinet.take_signed_proposal(h, r) {
            self.handle_signed_proposal(signed_proposal).await?;
        }
        Ok(())
    }

    fn check_proposal(&self, p: &Proposal<B>) -> OverlordResult<()> {
        if p.height != p.block.get_height() || p.block_hash != p.block.get_block_hash() {
            return Err(OverlordError::byz_block());
        }

        if self.prepare.pre_hash != p.block.get_pre_hash() {
            return Err(OverlordError::byz_block());
        }

        self.auth.verify_proof(p.block.get_proof())?;

        if let Some(lock) = &p.lock {
            self.auth.verify_pre_vote_qc(&lock)?;
        }
        Ok(())
    }

    async fn check_block(&self, block: &B) -> OverlordResult<()> {
        let exec_h = block.get_exec_height();
        if self.prepare.exec_height < exec_h {
            return Err(OverlordError::warn_block());
        }
        self.adapter
            .check_block(
                Context::new(),
                block,
                &self.prepare.get_block_states_list(exec_h),
            )
            .await
            .map_err(OverlordError::byz_adapter_check_block)
    }

    async fn create_block(&self) -> OverlordResult<B> {
        let height = self.state.stage.height;
        let exec_height = self.prepare.exec_height;
        let pre_hash = self.prepare.pre_hash.clone();
        let pre_proof = self.prepare.pre_proof.clone();
        let block_states = self.prepare.get_block_states_list(exec_height);
        self.adapter
            .create_block(
                Context::default(),
                height,
                exec_height,
                pre_hash,
                pre_proof,
                block_states,
            )
            .await
            .map_err(OverlordError::local_create_block)
    }

    async fn create_signed_proposal(&self) -> OverlordResult<SignedProposal<B>> {
        let height = self.state.stage.height;
        let round = self.state.stage.round;
        let proposer = self.auth.fixed_config.address.clone();
        let proposal = if let Some(lock) = &self.state.lock {
            let block = self
                .state
                .block
                .as_ref()
                .expect("Unreachable! Block is none when lock is some");
            let hash = lock.vote.block_hash.clone();
            Proposal::new(
                height,
                round,
                block.clone(),
                hash,
                Some(lock.clone()),
                proposer,
            )
        } else {
            let block = self.create_block().await?;
            let hash = block.get_block_hash();
            Proposal::new(height, round, block, hash, None, proposer)
        };
        self.auth.sign_proposal(proposal)
    }

    fn filter_msg(
        &mut self,
        height: Height,
        round: Round,
        capsule: &Capsule<B>,
    ) -> OverlordResult<()> {
        let my_height = self.state.stage.height;
        let my_round = self.state.stage.round;
        if height < my_height {
            return Err(OverlordError::debug_old());
        } else if height == my_height && round < my_round {
            if let Capsule::SignedProposal(_) = capsule {
                return Ok(());
            }
            return Err(OverlordError::debug_old());
        } else if height > my_height + HEIGHT_WINDOW || round > my_round + ROUND_WINDOW {
            return Err(OverlordError::net_much_high());
        } else if height > my_height {
            self.cabinet.insert(height, round, capsule.clone())?;
            return Err(OverlordError::debug_high());
        }
        Ok(())
    }
}

async fn recover_state_by_adapter<A: Adapter<B, S>, B: Blk, S: St>(
    adapter: &Arc<A>,
) -> StateInfo<B> {
    let height = adapter.get_latest_height(Context::default()).await.expect(
        "Cannot get the latest height from the adapter! It's meaningless to continue running",
    );
    StateInfo::from_height(height)
}

async fn recover_propose_prepare_and_config<A: Adapter<B, S>, B: Blk, S: St>(
    adapter: &Arc<A>,
    latest_height: Height,
) -> ProposePrepare<S> {
    let (block, proof) = get_block_with_proof(adapter, latest_height)
        .await
        .unwrap()
        .unwrap();
    let hash = block.get_block_hash();
    let exec_height = block.get_exec_height();
    let mut exec_results = vec![];

    let start_height = if exec_height == 0 {
        exec_height
    } else {
        exec_height + 1
    };

    for h in start_height..=latest_height {
        let exec_result = get_exec_result(adapter, h).await.unwrap().unwrap();
        exec_results.push(exec_result.clone());
    }

    ProposePrepare::new(latest_height, exec_results, proof, hash)
}

async fn get_block_with_proof<A: Adapter<B, S>, B: Blk, S: St>(
    adapter: &Arc<A>,
    height: Height,
) -> OverlordResult<Option<(B, Proof)>> {
    let vec = adapter
        .get_block_with_proofs(Context::default(), HeightRange::new(height, 1))
        .await
        .map_err(OverlordError::local_get_block)?;
    if vec.is_empty() {
        return Ok(None);
    }
    Ok(Some(vec[0].clone()))
}

async fn get_exec_result<A: Adapter<B, S>, B: Blk, S: St>(
    adapter: &Arc<A>,
    height: Height,
) -> OverlordResult<Option<ExecResult<S>>> {
    let opt = get_block_with_proof(adapter, height).await?;
    if let Some((block, proof)) = opt {
        let full_block = adapter
            .fetch_full_block(Context::default(), block.clone())
            .await
            .map_err(|_| OverlordError::net_fetch(block.get_block_hash()))?;
        let rst = adapter
            .save_and_exec_block_with_proof(Context::default(), height, full_block, proof.clone())
            .await
            .map_err(OverlordError::local_exec)?;
        Ok(Some(rst))
    } else {
        Ok(None)
    }
}

pub struct EventAgent<A: Adapter<B, S>, B: Blk, S: St> {
    adapter:     Arc<A>,
    time_config: TimeConfig,
    start_time:  Instant, // start time of current height
    fetch_set:   HashSet<Hash>,

    from_net: UnboundedReceiver<WrappedOverlordMsg<B>>,

    from_exec: UnboundedReceiver<ExecResult<S>>,
    to_exec:   UnboundedSender<ExecRequest>,

    from_fetch: UnboundedReceiver<OverlordResult<FetchedFullBlock>>,
    to_fetch:   UnboundedSender<OverlordResult<FetchedFullBlock>>,

    from_timeout: UnboundedReceiver<TimeoutEvent>,
    to_timeout:   UnboundedSender<TimeoutEvent>,
}

impl<A: Adapter<B, S>, B: Blk, S: St> EventAgent<A, B, S> {
    fn new(
        adapter: &Arc<A>,
        time_config: TimeConfig,
        from_net: UnboundedReceiver<(Context, OverlordMsg<B>)>,
        from_exec: UnboundedReceiver<ExecResult<S>>,
        to_exec: UnboundedSender<ExecRequest>,
    ) -> Self {
        let (to_fetch, from_fetch) = unbounded();
        let (to_timeout, from_timeout) = unbounded();
        EventAgent {
            adapter: Arc::<A>::clone(adapter),
            fetch_set: HashSet::new(),
            start_time: Instant::now(),
            time_config,
            from_net,
            from_exec,
            to_exec,
            from_fetch,
            to_fetch,
            from_timeout,
            to_timeout,
        }
    }

    fn next_height(&mut self, time_config: TimeConfig) {
        self.time_config = time_config;
        self.fetch_set.clear();
        self.start_time = Instant::now();
    }

    async fn transmit(&self, to: Address, msg: OverlordMsg<B>) -> OverlordResult<()> {
        self.adapter
            .transmit(Context::default(), to, msg)
            .await
            .map_err(OverlordError::net_transmit)
    }

    async fn broadcast(&self, msg: OverlordMsg<B>) -> OverlordResult<()> {
        self.adapter
            .broadcast(Context::default(), msg)
            .await
            .map_err(OverlordError::local_broadcast)
    }

    fn handle_fetch(
        &mut self,
        fetch_result: OverlordResult<FetchedFullBlock>,
    ) -> OverlordResult<FetchedFullBlock> {
        if let Err(error) = fetch_result {
            if let ErrorInfo::FetchFullBlock(hash) = error.info {
                self.fetch_set.remove(&hash);
                return Err(OverlordError::net_fetch(hash));
            }
            unreachable!()
        } else {
            Ok(fetch_result.unwrap())
        }
    }

    fn request_full_block(&self, block: B) {
        let block_hash = block.get_block_hash();
        if self.fetch_set.contains(&block_hash) {
            return;
        }

        let adapter = Arc::<A>::clone(&self.adapter);
        let to_fetch = self.to_fetch.clone();
        let height = block.get_height();

        tokio::spawn(async move {
            let rst = adapter
                .fetch_full_block(Context::default(), block)
                .await
                .map(|full_block| FetchedFullBlock::new(height, block_hash.clone(), full_block))
                .map_err(|_| OverlordError::net_fetch(block_hash));
            to_fetch
                .unbounded_send(rst)
                .expect("Fetch Channel is down! It's meaningless to continue running");
        });
    }

    fn save_and_exec_block(&self, request: ExecRequest) {
        self.to_exec
            .unbounded_send(request)
            .expect("Exec Channel is down! It's meaningless to continue running");
    }

    fn set_timeout(&self, stage: Stage) -> bool {
        let opt = self.compute_timeout(&stage);
        if let Some(interval) = opt {
            let timeout_info = TimeoutInfo::new(interval, stage.into(), self.to_timeout.clone());
            tokio::spawn(async move {
                timeout_info.await;
            });
            return true;
        }
        false
    }

    fn compute_timeout(&self, stage: &Stage) -> Option<Duration> {
        let config = &self.time_config;
        match stage.step {
            Step::Propose => {
                let timeout =
                    Duration::from_millis(config.interval * config.propose_ratio / TIME_DIVISOR);
                Some(apply_power(timeout, stage.round as u32))
            }
            Step::PreVote => {
                let timeout =
                    Duration::from_millis(config.interval * config.pre_vote_ratio / TIME_DIVISOR);
                Some(apply_power(timeout, stage.round as u32))
            }
            Step::PreCommit => {
                let timeout =
                    Duration::from_millis(config.interval * config.pre_commit_ratio / TIME_DIVISOR);
                Some(apply_power(timeout, stage.round as u32))
            }
            Step::Brake => Some(Duration::from_millis(
                config.interval * config.brake_ratio / TIME_DIVISOR,
            )),
            Step::Commit => {
                let cost = Instant::now() - self.start_time;
                cost.checked_sub(Duration::from_millis(config.interval))
            }
        }
    }
}

fn apply_power(timeout: Duration, power: u32) -> Duration {
    let mut timeout = timeout;
    let mut power = power;
    if power > POWER_CAP {
        power = POWER_CAP;
    }
    timeout *= 2u32.pow(power);
    timeout
}
