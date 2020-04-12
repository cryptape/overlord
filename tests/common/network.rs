use std::collections::HashMap;
use std::error::Error;

use bytes::Bytes;
use creep::Context;
use futures::channel::mpsc::{unbounded, UnboundedSender};
use overlord::{Address, OverlordMsg};
use parking_lot::RwLock;

use crate::common::block::Block;
use overlord::types::{AggregatedVote, SignedProposal};

type OverlordSender = UnboundedSender<(Context, OverlordMsg<Block>)>;

#[allow(dead_code)]
#[derive(Default)]
pub struct Network {
    handlers: RwLock<HashMap<Address, OverlordSender>>,
}

impl Network {
    pub fn register(
        &self,
        address: Address,
        sender: UnboundedSender<(Context, OverlordMsg<Block>)>,
    ) {
        let mut handlers = self.handlers.write();
        handlers.insert(address, sender);
    }

    pub fn broadcast(
        &self,
        from: &Address,
        msg: OverlordMsg<Block>,
    ) -> Result<(), Box<dyn Error + Send>> {
        self.handlers
            .read()
            .iter()
            .filter(|(address, _)| address != &from)
            .for_each(|(_, sender)| {
                let _ = sender.unbounded_send((Context::default(), msg.clone()));
            });

        Ok(())
    }

    pub fn transmit(
        &self,
        to: &Address,
        msg: OverlordMsg<Block>,
    ) -> Result<(), Box<dyn Error + Send>> {
        let handler = self.handlers.read();
        let sender = handler.get(to).unwrap();
        let _ = sender.unbounded_send((Context::default(), msg));
        Ok(())
    }
}

#[test]
fn test_network() {
    let addresses: Vec<Bytes> = vec![
        "77667feeaccdc991f0f21182bd04ba7277c881c1".to_owned(),
        "82fa6a3978aae4e7527c6a10e9cff9c4b018053e".to_owned(),
        "5dc3a5d4246d0468e1f0ac3776607df40481bbf6".to_owned(),
        "fd6d62572ec57829485c78f9febe2cb18438332c".to_owned(),
    ]
    .iter()
    .map(|address| Bytes::from(hex::decode(address).unwrap()))
    .collect();

    let (sender_0, mut receiver_0) = unbounded();
    let (sender_1, mut receiver_1) = unbounded();
    let (sender_2, mut receiver_2) = unbounded();

    let network = Network::default();
    // test register
    network.register(addresses[0].clone(), sender_0);
    network.register(addresses[1].clone(), sender_1);

    // test broadcast
    let msg = OverlordMsg::SignedProposal(SignedProposal::default());
    network.broadcast(&addresses[0], msg.clone()).unwrap();
    assert!(receiver_0.try_next().is_err());
    assert_eq!(receiver_1.try_next().unwrap().unwrap().1, msg);
    assert!(receiver_2.try_next().is_err());

    // test transmit
    let msg = OverlordMsg::AggregatedVote(AggregatedVote::default());
    network.transmit(&addresses[0], msg.clone()).unwrap();
    assert_eq!(receiver_0.try_next().unwrap().unwrap().1, msg);
    assert!(receiver_1.try_next().is_err());
    assert!(receiver_2.try_next().is_err());

    // test register new address
    network.register(addresses[2].clone(), sender_2);
    let msg = OverlordMsg::SignedProposal(SignedProposal::default());
    network.broadcast(&addresses[0], msg.clone()).unwrap();
    assert!(receiver_0.try_next().is_err());
    assert_eq!(receiver_1.try_next().unwrap().unwrap().1, msg);
    assert_eq!(receiver_2.try_next().unwrap().unwrap().1, msg);

    // test sender drop
    {
        let (sender_3, _) = unbounded();
        network.register(addresses[3].clone(), sender_3);
    }
    let msg = OverlordMsg::AggregatedVote(AggregatedVote::default());
    network.transmit(&addresses[3], msg).unwrap();
}
