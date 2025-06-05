use std::{
    borrow::Borrow,
    collections::BTreeMap,
    env,
    num::NonZeroUsize,
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc::{sync_channel, Receiver, SyncSender},
        Arc, Mutex, OnceLock,
    },
    thread, time::Duration,
};

use anyhow::{anyhow, Result};
use alloy_provider::{network::Ethereum, Provider};
use clap::Parser;
use cli::Args;
use tracing::{error, info, info_span, instrument, warn};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
use store::localdb::LocalDB;
use zkm_sdk::{include_elf, ExecutionReport, ProverClient, ZKMProof, ZKMProofWithPublicValues, ZKMProvingKey, ZKMPublicValues, ZKMVerifyingKey};

use crate::db::*;

mod cli;
mod db;

const AGGREGATION_ELF: &[u8] = include_elf!("guest-aggregation");

struct AggreationInput((Proof, Proof));

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProofWithPublicValues {
    pub proof: ZKMProof,
    pub public_values: ZKMPublicValues,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    // Initialize the environment variables.
    dotenv::dotenv().ok();

    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "info");
    }

    // Initialize the logger.
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(
            EnvFilter::from_default_env()
                .add_directive("zkm_core_machine=warn".parse().unwrap())
                .add_directive("zkm_core_executor=warn".parse().unwrap())
                .add_directive("zkm_prover=warn".parse().unwrap()),
        )
        .init();

    let args = Args::parse();

    let local_db = LocalDB::new(&format!("sqlite:{}", args.database_url), true).await;
    let sqlite_db = db::PersistToDB::new(&local_db).await;
    let local_db = Arc::new(local_db);

    // Initialize the proving client.
    let client = Arc::new(ProverClient::new());

    // Setup the proving and verifying keys.
    let (pk, vk) = client.setup(AGGREGATION_ELF);

    let (block_number_tx, block_number_rx) = sync_channel::<u64>(2);
    let (input_tx, input_rx) = sync_channel::<AggreationInput>(1);
    let (proof_tx, proof_rx) = sync_channel::<ProofWithPublicValues>(1);

    tokio::spawn(data_preparer(local_db.clone(), &vk, block_number_rx, input_tx, proof_rx));
    std::thread::spawn(move || {
        proof_aggregator(local_db.clone(), client.clone(),&vk, block_number_tx, input_rx, proof_tx);
    });

    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.unwrap();
        drop(block_number_tx);
        drop(input_tx);
        drop(proof_tx);
    });

    Ok(())
}

async fn data_preparer(
    db: Arc<LocalDB>,
    vk: &ZKMVerifyingKey,
    block_number_rx: Receiver<u64>,
    input_tx: SyncSender<AggreationInput>,
    proof_rx: Receiver<ProofWithPublicValues>,
) -> Result<()> {
    let mut restart = true;

    loop {
        let block_number = block_number_rx.recv();
        if let Ok(block_number) = block_number {
            on_aggregation_start(db, block_number).await?;

            let agg_input = match block_number {
                2 => {
                    restart = false;
                    let block_proof1 = load_proof(db, 1, false).await?;
                    let block_proof2 = load_proof(db, 2, false).await?;
                    AggreationInput((block_proof1, block_proof2))
                }
                n if n > 2 => {
                    let block_proof = load_proof(db, block_number, false).await?;
                    let pre_agg_proof = if restart {
                        restart = false;
                        load_proof(db, block_number - 1, true).await?
                    } else {
                        let agg_proof = proof_rx.recv()?;
                        Proof {
                            block_number: block_number - 1,
                            proof: agg_proof.proof,
                            public_values: agg_proof.public_values,
                            vk: vk.clone(),
                        }
                    };
                    AggreationInput((block_proof, pre_agg_proof))
                }
                _ => panic!("block number >= 2"),
            };

            input_tx.send((block_number, agg_input))?;
        } else {
            break;
        }
    }
}

async fn proof_aggregator(
    db: Arc<LocalDB>,
    client: Arc<ProverClient>,
    vk: &ZKMVerifyingKey,
    block_number_tx: SyncSender<u64>,
    input_rx: Receiver<AggreationInput>,
    proof_tx: SyncSender<ProofWithPublicValues>,
) -> Result<()> {
    loop {
        let proofs = input_rx.recv();
        if let Ok(proofs) = proofs {
            block_number_tx.send(block_number + 1)?;

            if let Ok((agg_proof, exec_report, proving_duration)) = generate_aggregation_proof(client.clone(), &buffer) {
                info!("Successfully processed block {}", block_number);
                on_aggregation_end(db, block_number, &agg_proof, &vk, exec_report, proving_duration).await?;

                proof_tx.send(ProofWithPublicValues {
                    proof: agg_proof.proof,
                    public_values: agg_proof.public_values,
                })?;
            } else {
                on_aggregation_failed(db, block_number, proving_duration).await?;
            }
        } else {
            break;
        }
    }
    Ok(())
}

async fn generate_aggregation_proof(
    client: Arc<ProverClient>,
    inputs: Vec<Proof>,
) -> Result<(ZKMProofWithPublicValues, ExecutionReport, Duration)> {
    let mut stdin = ZKMStdin::new();

    // Write the block numbers.
    let block_numbers = inputs.iter().map(|input| input.block_number).collect::<Vec<_>>();
    stdin.write::<Vec<u64>>(&block_numbers);

    // Write the verification keys.
    let vkeys = inputs.iter().map(|input| input.vk.hash_u32()).collect::<Vec<_>>();
    stdin.write::<Vec<[u32; 8]>>(&vkeys);

    // Write the public values.
    let public_values =
        inputs.iter().map(|input| input.public_values.to_vec()).collect::<Vec<_>>();
    stdin.write::<Vec<Vec<u8>>>(&public_values);

    // Write the proofs.
    //
    // Note: this data will not actually be read by the aggregation program, instead it will be
    // witnessed by the prover during the recursive aggregation process inside zkMIPS itself.
    for input in inputs {
        let ZKMProof::Compressed(proof) = input.proof else { panic!() };
        stdin.write_proof(*proof, input.vk);
    }

    // Execution in zkMIPS is a long-running, blocking task, so run it in a separate thread.
    let exec_report = task::spawn_blocking(move || {
        info_span!("execute_client", block_numbers.last().unwrap()).in_scope(|| {
            client.execute(AGGREGATION_ELF, &stdin);
        })
    })
    .await?;

    let proving_start = Instant::now();

    // Generate the aggregation proof.
    let agg_proof = client.prove(&aggregation_pk, stdin).compressed().run()?;

    let proving_duration = proving_start.elapsed();

    Ok((agg_proof, proving_duration, exec_report))
}
