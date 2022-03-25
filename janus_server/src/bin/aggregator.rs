use anyhow::{anyhow, Context, Result};
use chrono::Duration;
use deadpool_postgres::{Manager, Pool};
use janus_server::{
    aggregator::aggregator_server,
    datastore::Datastore,
    hpke::{HpkeRecipient, Label},
    message::Role,
    message::TaskId,
    time::RealClock,
    trace::install_subscriber,
};
use prio::vdaf::{prio3::Prio3Aes128Count, Vdaf};
use ring::{
    hmac::{self, HMAC_SHA256},
    rand::SystemRandom,
};
use std::{
    env::args,
    iter::Iterator,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    str::FromStr,
    sync::Arc,
};
use tokio_postgres::{Config, NoTls};
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    let role = match args().nth(1).as_deref() {
        None | Some("leader") => Role::Leader,
        Some("helper") => Role::Helper,
        Some(r) => {
            return Err(anyhow!("unsupported role {}", r));
        }
    };

    install_subscriber().context("failed to install tracing subscriber")?;

    // TODO(issue #20): We need to specify configuration parameters rather than hardcoding them.
    let task_id = TaskId::random();

    let vdaf = Prio3Aes128Count::new(2).unwrap();
    let verify_param = vdaf.setup().unwrap().1.first().unwrap().clone();

    let cfg = Config::from_str("postgres://postgres:postgres@localhost:5432/postgres")?;
    let conn_mgr = Manager::new(cfg, NoTls);
    let pool = Pool::builder(conn_mgr).build()?;
    let datastore = Arc::new(Datastore::new(pool));

    let hpke_recipient =
        HpkeRecipient::generate(task_id, Label::InputShare, Role::Client, Role::Leader);

    let agg_auth_key = hmac::Key::generate(HMAC_SHA256, &SystemRandom::new())
        .map_err(|_| anyhow!("couldn't generate agg_auth_key"))?;

    let listen_address = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 8080);

    let (bound_address, server) = aggregator_server(
        vdaf,
        datastore,
        RealClock::default(),
        Duration::minutes(10),
        role,
        verify_param,
        hpke_recipient,
        agg_auth_key,
        listen_address,
    )
    .context("failed to create aggregator server")?;
    info!(?task_id, ?bound_address, "running aggregator");

    server.await;

    Ok(())
}
