mod common;

use std::io::Write;
use std::thread;
use std::time::Duration;

use chrono::Local;
use log::LevelFilter;

use crate::common::platform::Platform;

#[tokio::test(threaded_scheduler)]
async fn test_4_nodes() {
    let _ = env_logger::builder()
        .format(|buf, record| {
            writeln!(
                buf,
                "{} [{}] - {}",
                Local::now().format("%Y-%m-%dT%H:%M:%S.%f"),
                record.level(),
                record.args()
            )
        })
        .filter_level(LevelFilter::Debug)
        .is_test(true)
        .try_init();
    let platform = Platform::new(4);
    platform.run();
    thread::sleep(Duration::from_secs(20000));
}
