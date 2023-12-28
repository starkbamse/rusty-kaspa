use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use clap::{Arg, Command};
use itertools::Itertools;
use kaspa_addresses::Address;
use kaspa_consensus_core::{
    config::params::{TESTNET11_PARAMS, TESTNET_PARAMS},
    constants::TX_VERSION,
    sign::sign,
    subnets::SUBNETWORK_ID_NATIVE,
    tx::{MutableTransaction, Transaction, TransactionInput, TransactionOutpoint, TransactionOutput, UtxoEntry},
};
use kaspa_core::{info, kaspad_env::version, time::unix_now, warn};
use kaspa_grpc_client::GrpcClient;
use kaspa_rpc_core::{api::rpc::RpcApi, notify::mode::NotificationMode};
use kaspa_txscript::pay_to_address_script;
use parking_lot::RwLock;
use rayon::prelude::*;
use secp256k1::{rand::thread_rng, KeyPair};
use tokio::{
    sync::mpsc,
    time::{interval, MissedTickBehavior},
};

const DEFAULT_SEND_AMOUNT: u64 = 10_000;

const FEE_PER_MASS: u64 = 10;

struct Stats {
    num_txs: usize,
    num_utxos: usize,
    utxos_amount: u64,
    num_outs: usize,
    since: u64,
}

pub struct Args {
    pub private_key: Option<String>,
    pub tps: u64,
    pub rpc_server: String,
}

impl Args {
    fn parse() -> Self {
        let m = cli().get_matches();
        Args {
            private_key: m.get_one::<String>("private-key").cloned(),
            tps: m.get_one::<u64>("tps").cloned().unwrap(),
            rpc_server: m.get_one::<String>("rpcserver").cloned().unwrap_or("localhost:16210".to_owned()),
        }
    }
}

pub fn cli() -> Command {
    Command::new("rothschild")
        .about(format!("{} (rothschild) v{}", env!("CARGO_PKG_DESCRIPTION"), version()))
        .version(env!("CARGO_PKG_VERSION"))
        .arg(Arg::new("private-key").long("private-key").short('k').value_name("private-key").help("Private key in hex format"))
        .arg(
            Arg::new("tps")
                .long("tps")
                .short('t')
                .value_name("tps")
                .default_value("1")
                .value_parser(clap::value_parser!(u64))
                .help("Transactions per second"),
        )
        .arg(
            Arg::new("rpcserver")
                .long("rpcserver")
                .short('s')
                .value_name("rpcserver")
                .default_value("localhost:16210")
                .help("RPC server"),
        )
}

#[tokio::main]
async fn main() {
    kaspa_core::log::init_logger(None, "");
    let args = Args::parse();
    let rpc_client = GrpcClient::connect(
        NotificationMode::Direct,
        format!("grpc://{}", args.rpc_server),
        true,
        None,
        false,
        Some(500_000),
        Default::default(),
    )
    .await
    .unwrap();
    info!("Connected to RPC");
    let pending = Arc::new(RwLock::new(HashMap::new()));

    let schnorr_key = if let Some(private_key_hex) = args.private_key {
        let mut private_key_bytes = [0u8; 32];
        faster_hex::hex_decode(private_key_hex.as_bytes(), &mut private_key_bytes).unwrap();
        secp256k1::KeyPair::from_seckey_slice(secp256k1::SECP256K1, &private_key_bytes).unwrap()
    } else {
        let (sk, pk) = &secp256k1::generate_keypair(&mut thread_rng());
        let kaspa_addr =
            Address::new(kaspa_addresses::Prefix::Testnet, kaspa_addresses::Version::PubKey, &pk.x_only_public_key().0.serialize());
        info!(
            "Generated private key {} and address {}. Send some funds to this address and rerun rothschild with `--private-key {}`",
            sk.display_secret(),
            String::from(&kaspa_addr),
            sk.display_secret()
        );
        return;
    };

    let kaspa_addr = Address::new(
        kaspa_addresses::Prefix::Testnet,
        kaspa_addresses::Version::PubKey,
        &schnorr_key.x_only_public_key().0.serialize(),
    );

    info!("Using Rothschild with private key {} and address {}", schnorr_key.display_secret(), String::from(&kaspa_addr));
    let info = rpc_client.get_block_dag_info().await.unwrap();
    let coinbase_maturity = match info.network.suffix {
        Some(11) => TESTNET11_PARAMS.coinbase_maturity,
        None | Some(_) => TESTNET_PARAMS.coinbase_maturity,
    };
    info!(
        "Node block-DAG info: \n\tNetwork: {}, \n\tBlock count: {}, \n\tHeader count: {}, \n\tDifficulty: {}, 
\tMedian time: {}, \n\tDAA score: {}, \n\tPruning point: {}, \n\tTips: {}, \n\t{} virtual parents: ...{}, \n\tCoinbase maturity: {}",
        info.network,
        info.block_count,
        info.header_count,
        info.difficulty,
        info.past_median_time,
        info.virtual_daa_score,
        info.pruning_point_hash,
        info.tip_hashes.len(),
        info.virtual_parent_hashes.len(),
        info.virtual_parent_hashes.last().unwrap(),
        coinbase_maturity,
    );

    let (submit_tx_send, submit_tx_recv) = mpsc::channel(100);

    let mut utxos = refresh_utxos(&rpc_client, kaspa_addr.clone(), pending.clone(), coinbase_maturity).await;
    let utxos_len = Arc::new(AtomicUsize::new(utxos.len()));

    {
        let rpc_client = rpc_client.clone();
        let pending = pending.clone();
        let utxos_len = utxos_len.clone();
        tokio::spawn(async move { submit_loop(submit_tx_recv, schnorr_key, rpc_client, pending, utxos_len).await });
    }

    let mut ticker = interval(Duration::from_secs_f64(1.0 / (args.tps as f64)));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut maximize_inputs = false;
    let mut last_refresh = unix_now();
    loop {
        ticker.tick().await;
        maximize_inputs = should_maximize_inputs(maximize_inputs, &utxos, &pending.read());
        let now = unix_now();
        let has_funds = match maybe_send_tx(kaspa_addr.clone(), &mut utxos, pending.clone(), maximize_inputs).await {
            Some(tx) => {
                submit_tx_send.send(tx).await.unwrap();
                true
            }
            None => false,
        };
        if !has_funds {
            info!("Has not enough funds");
        }
        if !has_funds || now - last_refresh > 60_000 {
            info!("Refetching UTXO set");
            tokio::time::sleep(Duration::from_millis(100)).await; // We don't want this operation to be too frequent since it's heavy on the node, so we wait some time before executing it.
            utxos = refresh_utxos(&rpc_client, kaspa_addr.clone(), pending.clone(), coinbase_maturity).await;
            utxos_len.store(utxos.len(), Ordering::Relaxed);
            last_refresh = unix_now();
            pause_if_mempool_is_full(&rpc_client).await;
        }
        clean_old_pending_outpoints(&mut pending.write());
    }
}

struct TxToSign {
    tx: Transaction,
    utxos: Box<[(TransactionOutpoint, UtxoEntry)]>,
}

async fn submit_loop(
    mut submit_tx_recv: mpsc::Receiver<TxToSign>,
    schnorr_key: KeyPair,
    rpc_client: GrpcClient,
    pending: Arc<RwLock<HashMap<TransactionOutpoint, u64>>>,
    utxos_len: Arc<AtomicUsize>,
) {
    let mut stats = Stats { num_txs: 0, since: unix_now(), num_utxos: 0, utxos_amount: 0, num_outs: 0 };
    let num_cpus = num_cpus::get();
    loop {
        match submit_tx_recv.recv().await {
            Some(tx) => {
                let mut chunk = Vec::with_capacity(num_cpus);
                chunk.push(tx);
                for _ in 1..num_cpus {
                    match submit_tx_recv.try_recv() {
                        Ok(tx) => chunk.push(tx),
                        Err(_) => break,
                    }
                }
                let signed_txs: Vec<_> = chunk
                    .into_par_iter()
                    .map(|tx| {
                        let signed_tx = sign(
                            MutableTransaction::with_entries(tx.tx, tx.utxos.iter().map(|(_, entry)| entry.clone()).collect_vec()),
                            schnorr_key,
                        );

                        let amount_used = tx.utxos.into_iter().map(|(_, entry)| entry.amount).sum::<u64>();
                        (signed_tx.tx, amount_used)
                    })
                    .collect();
                for (tx, amount_used) in signed_txs {
                    match rpc_client.submit_transaction((&tx).into(), false).await {
                        Ok(_) => {}
                        Err(e) => {
                            warn!("RPC error when submitting {}: {}", tx.id(), e);
                            continue;
                        }
                    }

                    stats.num_txs += 1;
                    stats.num_utxos += tx.inputs.len();
                    stats.utxos_amount += amount_used;
                    stats.num_outs += tx.outputs.len();
                    let now = unix_now();
                    let time_past = now - stats.since;
                    if time_past > 50_000 {
                        let pending_len = pending.read().len();
                        let utxos_len = utxos_len.load(Ordering::SeqCst);
                        info!(
                            "Tx rate: {:.1}/sec, avg UTXO amount: {}, avg UTXOs per tx: {}, avg outs per tx: {}, estimated available UTXOs: {}",
                            1000f64 * (stats.num_txs as f64) / (time_past as f64),
                            (stats.utxos_amount / stats.num_utxos as u64),
                            stats.num_utxos / stats.num_txs,
                            stats.num_outs / stats.num_txs,
                            if utxos_len > pending_len { utxos_len - pending_len } else { 0 },
                        );
                        stats.since = now;
                        stats.num_txs = 0;
                        stats.num_utxos = 0;
                        stats.utxos_amount = 0;
                        stats.num_outs = 0;
                    }
                }
            }
            None => return,
        }
    }
}

fn should_maximize_inputs(
    old_value: bool,
    utxos: &Vec<(TransactionOutpoint, UtxoEntry)>,
    pending: &HashMap<TransactionOutpoint, u64>,
) -> bool {
    let estimated_utxos = if utxos.len() > pending.len() { utxos.len() - pending.len() } else { 0 };
    if !old_value && estimated_utxos > 1_000_000 {
        info!("Starting to maximize inputs");
        true
    } else if old_value && estimated_utxos < 500_000 {
        info!("Stopping to maximize inputs");
        false
    } else {
        old_value
    }
}

async fn pause_if_mempool_is_full(rpc_client: &GrpcClient) {
    loop {
        let mempool_size = rpc_client.get_info().await.unwrap().mempool_size;
        if mempool_size < 10_000 {
            break;
        }

        const PAUSE_DURATION: u64 = 10;
        info!("Mempool has {} entries. Pausing for {} seconds to reduce mempool pressure", mempool_size, PAUSE_DURATION);
        tokio::time::sleep(Duration::from_secs(PAUSE_DURATION)).await;
    }
}

async fn refresh_utxos(
    rpc_client: &GrpcClient,
    kaspa_addr: Address,
    pending: Arc<RwLock<HashMap<TransactionOutpoint, u64>>>,
    coinbase_maturity: u64,
) -> Vec<(TransactionOutpoint, UtxoEntry)> {
    populate_pending_outpoints_from_mempool(rpc_client, kaspa_addr.clone(), pending).await;
    fetch_spendable_utxos(rpc_client, kaspa_addr, coinbase_maturity).await
}

async fn populate_pending_outpoints_from_mempool(
    rpc_client: &GrpcClient,
    kaspa_addr: Address,
    pending: Arc<RwLock<HashMap<TransactionOutpoint, u64>>>,
) {
    let entries = rpc_client.get_mempool_entries_by_addresses(vec![kaspa_addr], true, false).await.unwrap();
    let now = unix_now();
    let mut pending_write = pending.write();
    for entry in entries {
        for entry in entry.sending {
            for input in entry.transaction.inputs {
                pending_write.insert(input.previous_outpoint, now);
            }
        }
    }
}

async fn fetch_spendable_utxos(
    rpc_client: &GrpcClient,
    kaspa_addr: Address,
    coinbase_maturity: u64,
) -> Vec<(TransactionOutpoint, UtxoEntry)> {
    let resp = rpc_client.get_utxos_by_addresses(vec![kaspa_addr]).await.unwrap();
    let dag_info = rpc_client.get_block_dag_info().await.unwrap();
    let mut utxos = Vec::with_capacity(resp.len());
    for resp_entry in
        resp.into_iter().filter(|resp_entry| is_utxo_spendable(&resp_entry.utxo_entry, dag_info.virtual_daa_score, coinbase_maturity))
    {
        utxos.push((resp_entry.outpoint, resp_entry.utxo_entry));
    }
    utxos.sort_by(|a, b| b.1.amount.cmp(&a.1.amount));
    utxos
}

fn is_utxo_spendable(entry: &UtxoEntry, virtual_daa_score: u64, coinbase_maturity: u64) -> bool {
    let needed_confs = if !entry.is_coinbase {
        10
    } else {
        coinbase_maturity * 2 // TODO: We should compare with sink blue score in the case of coinbase
    };
    entry.block_daa_score + needed_confs < virtual_daa_score
}

async fn maybe_send_tx(
    kaspa_addr: Address,
    utxos: &mut Vec<(TransactionOutpoint, UtxoEntry)>,
    pending: Arc<RwLock<HashMap<TransactionOutpoint, u64>>>,
    maximize_inputs: bool,
) -> Option<TxToSign> {
    let num_outs = if maximize_inputs { 1 } else { 2 };
    let (selected_utxos, selected_amount) = select_utxos(utxos, DEFAULT_SEND_AMOUNT, num_outs, maximize_inputs, &pending.read());
    if selected_amount == 0 {
        return None;
    }

    let tx = generate_tx(&selected_utxos, selected_amount, num_outs, &kaspa_addr);

    let now = unix_now();
    {
        let mut pending_write = pending.write();
        for input in tx.inputs.iter() {
            pending_write.insert(input.previous_outpoint, now);
        }
    }

    Some(TxToSign { tx, utxos: selected_utxos.into() })
}

fn clean_old_pending_outpoints(pending: &mut HashMap<TransactionOutpoint, u64>) {
    let now = unix_now();
    let old_keys = pending.iter().filter(|(_, time)| now - *time > 3600 * 1000).map(|(op, _)| *op).collect_vec();
    for key in old_keys {
        pending.remove(&key).unwrap();
    }
}

fn required_fee(num_utxos: usize, num_outs: u64) -> u64 {
    FEE_PER_MASS * estimated_mass(num_utxos, num_outs)
}

fn estimated_mass(num_utxos: usize, num_outs: u64) -> u64 {
    200 + 34 * num_outs + 1000 * (num_utxos as u64)
}

fn generate_tx(utxos: &[(TransactionOutpoint, UtxoEntry)], send_amount: u64, num_outs: u64, kaspa_addr: &Address) -> Transaction {
    let script_public_key = pay_to_address_script(kaspa_addr);
    let inputs = utxos
        .iter()
        .map(|(op, _)| TransactionInput { previous_outpoint: *op, signature_script: vec![], sequence: 0, sig_op_count: 1 })
        .collect_vec();

    let outputs = (0..num_outs)
        .map(|_| TransactionOutput { value: send_amount / num_outs, script_public_key: script_public_key.clone() })
        .collect_vec();
    let unsigned_tx = Transaction::new(TX_VERSION, inputs, outputs, 0, SUBNETWORK_ID_NATIVE, 0, vec![]);
    unsigned_tx
}

fn select_utxos(
    utxos: &[(TransactionOutpoint, UtxoEntry)],
    min_amount: u64,
    num_outs: u64,
    maximize_utxos: bool,
    pending: &HashMap<TransactionOutpoint, u64>,
) -> (Vec<(TransactionOutpoint, UtxoEntry)>, u64) {
    const MAX_UTXOS: usize = 84;
    let mut selected_amount: u64 = 0;
    let mut selected = Vec::new();
    for (outpoint, entry) in utxos.iter().filter(|(op, _)| !pending.contains_key(op)).cloned() {
        selected_amount += entry.amount;
        selected.push((outpoint, entry));

        let fee = required_fee(selected.len(), num_outs);

        if selected_amount >= min_amount + fee && (!maximize_utxos || selected.len() == MAX_UTXOS) {
            return (selected, selected_amount - fee);
        }

        if selected.len() > MAX_UTXOS {
            return (vec![], 0);
        }
    }

    (vec![], 0)
}
