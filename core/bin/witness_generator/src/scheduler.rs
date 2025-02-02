use std::convert::TryInto;

use std::time::Instant;

use async_trait::async_trait;
use circuit_definitions::boojum::field::goldilocks::{GoldilocksExt2, GoldilocksField};
use circuit_definitions::boojum::gadgets::recursion::recursive_tree_hasher::CircuitGoldilocksPoseidon2Sponge;
use circuit_definitions::circuit_definitions::recursion_layer::scheduler::SchedulerCircuit;
use circuit_definitions::circuit_definitions::recursion_layer::{
    ZkSyncRecursionLayerStorageType, ZkSyncRecursionLayerVerificationKey, ZkSyncRecursionProof,
    ZkSyncRecursiveLayerCircuit, SCHEDULER_CAPACITY,
};
use circuit_definitions::recursion_layer_proof_config;
use circuit_definitions::zkevm_circuits::scheduler::input::SchedulerCircuitInstanceWitness;
use circuit_definitions::zkevm_circuits::scheduler::SchedulerConfig;
use zksync_vk_setup_data_server_fri::get_recursive_layer_vk_for_circuit_type;
use zksync_vk_setup_data_server_fri::utils::get_leaf_vk_params;

use crate::utils::{
    load_proofs_for_job_ids, CircuitWrapper, FriProofWrapper, SchedulerPartialInputWrapper,
};
use zksync_dal::ConnectionPool;
use zksync_object_store::{FriCircuitKey, ObjectStore, ObjectStoreFactory};
use zksync_queued_job_processor::JobProcessor;
use zksync_types::proofs::AggregationRound;
use zksync_types::L1BatchNumber;

pub struct SchedulerArtifacts {
    scheduler_circuit: ZkSyncRecursiveLayerCircuit,
}

#[derive(Clone)]
pub struct SchedulerWitnessGeneratorJob {
    block_number: L1BatchNumber,
    scheduler_witness: SchedulerCircuitInstanceWitness<
        GoldilocksField,
        CircuitGoldilocksPoseidon2Sponge,
        GoldilocksExt2,
    >,
    node_vk: ZkSyncRecursionLayerVerificationKey,
}

#[derive(Debug)]
pub struct SchedulerWitnessGenerator {
    object_store: Box<dyn ObjectStore>,
    prover_connection_pool: ConnectionPool,
}

impl SchedulerWitnessGenerator {
    pub async fn new(
        store_factory: &ObjectStoreFactory,
        prover_connection_pool: ConnectionPool,
    ) -> Self {
        Self {
            object_store: store_factory.create_store().await,
            prover_connection_pool,
        }
    }

    fn process_job_sync(
        job: SchedulerWitnessGeneratorJob,
        started_at: Instant,
    ) -> SchedulerArtifacts {
        vlog::info!(
            "Starting fri witness generation of type {:?} for block {}",
            AggregationRound::Scheduler,
            job.block_number.0
        );
        let config = SchedulerConfig {
            proof_config: recursion_layer_proof_config(),
            vk_fixed_parameters: job.node_vk.into_inner().fixed_parameters,
            capacity: SCHEDULER_CAPACITY,
            _marker: std::marker::PhantomData,
        };

        let scheduler_circuit = SchedulerCircuit {
            witness: job.scheduler_witness,
            config,
            transcript_params: (),
            _marker: std::marker::PhantomData,
        };
        metrics::histogram!(
                    "prover_fri.witness_generation.witness_generation_time",
                    started_at.elapsed(),
                    "aggregation_round" => format!("{:?}", AggregationRound::Scheduler),
        );

        vlog::info!(
            "Scheduler generation for block {} is complete in {:?}",
            job.block_number.0,
            started_at.elapsed()
        );

        SchedulerArtifacts {
            scheduler_circuit: ZkSyncRecursiveLayerCircuit::SchedulerCircuit(scheduler_circuit),
        }
    }
}

#[async_trait]
impl JobProcessor for SchedulerWitnessGenerator {
    type Job = SchedulerWitnessGeneratorJob;
    type JobId = L1BatchNumber;
    type JobArtifacts = SchedulerArtifacts;

    const SERVICE_NAME: &'static str = "fri_scheduler_witness_generator";

    async fn get_next_job(&self) -> Option<(Self::JobId, Self::Job)> {
        let mut prover_connection = self.prover_connection_pool.access_storage().await;

        let l1_batch_number = prover_connection
            .fri_witness_generator_dal()
            .get_next_scheduler_witness_job()
            .await?;
        let proof_job_ids = prover_connection
            .fri_scheduler_dependency_tracker_dal()
            .get_final_prover_job_ids_for(l1_batch_number)
            .await;
        let started_at = Instant::now();
        let proofs = load_proofs_for_job_ids(&proof_job_ids, &*self.object_store).await;
        metrics::histogram!(
                    "prover_fri.witness_generation.blob_fetch_time",
                    started_at.elapsed(),
                    "aggregation_round" => format!("{:?}", AggregationRound::Scheduler),
        );
        let recursive_proofs = proofs
            .into_iter()
            .map(|wrapper| match wrapper {
                FriProofWrapper::Base(_) => {
                    panic!(
                        "Expected only recursive proofs for scheduler l1 batch {}",
                        l1_batch_number
                    )
                }
                FriProofWrapper::Recursive(recursive_proof) => recursive_proof.into_inner(),
            })
            .collect::<Vec<_>>();
        Some((
            l1_batch_number,
            prepare_job(l1_batch_number, recursive_proofs, &*self.object_store).await,
        ))
    }

    async fn save_failure(&self, job_id: L1BatchNumber, _started_at: Instant, error: String) -> () {
        self.prover_connection_pool
            .access_storage()
            .await
            .fri_witness_generator_dal()
            .mark_scheduler_job_failed(&error, job_id)
            .await;
    }

    #[allow(clippy::async_yields_async)]
    async fn process_job(
        &self,
        job: SchedulerWitnessGeneratorJob,
        started_at: Instant,
    ) -> tokio::task::JoinHandle<SchedulerArtifacts> {
        tokio::task::spawn_blocking(move || Self::process_job_sync(job, started_at))
    }

    async fn save_result(
        &self,
        job_id: L1BatchNumber,
        started_at: Instant,
        artifacts: SchedulerArtifacts,
    ) {
        let key = FriCircuitKey {
            block_number: job_id,
            circuit_id: 1,
            sequence_number: 0,
            depth: 0,
            aggregation_round: AggregationRound::Scheduler,
        };
        let blob_save_started_at = Instant::now();
        let scheduler_circuit_blob_url = self
            .object_store
            .put(key, &CircuitWrapper::Recursive(artifacts.scheduler_circuit))
            .await
            .unwrap();
        metrics::histogram!(
                    "prover_fri.witness_generation.blob_save_time",
                    blob_save_started_at.elapsed(),
                    "aggregation_round" => format!("{:?}", AggregationRound::Scheduler),
        );

        let mut prover_connection = self.prover_connection_pool.access_storage().await;
        let mut transaction = prover_connection.start_transaction().await;
        transaction
            .fri_prover_jobs_dal()
            .insert_prover_job(
                job_id,
                1,
                0,
                0,
                AggregationRound::Scheduler,
                &scheduler_circuit_blob_url,
                false,
            )
            .await;

        transaction
            .fri_witness_generator_dal()
            .mark_scheduler_job_as_successful(job_id, started_at.elapsed())
            .await;

        transaction.commit().await;
    }
}

async fn prepare_job(
    l1_batch_number: L1BatchNumber,
    proofs: Vec<ZkSyncRecursionProof>,
    object_store: &dyn ObjectStore,
) -> SchedulerWitnessGeneratorJob {
    let started_at = Instant::now();
    let node_vk = get_recursive_layer_vk_for_circuit_type(
        ZkSyncRecursionLayerStorageType::NodeLayerCircuit as u8,
    );
    let SchedulerPartialInputWrapper(mut scheduler_witness) =
        object_store.get(l1_batch_number).await.unwrap();
    scheduler_witness.node_layer_vk_witness = node_vk.clone().into_inner();

    scheduler_witness.proof_witnesses = proofs.into();

    let leaf_vk_commits = get_leaf_vk_params();
    let leaf_layer_params = leaf_vk_commits
        .iter()
        .map(|el| el.1.clone())
        .collect::<Vec<_>>()
        .try_into()
        .unwrap();
    scheduler_witness.leaf_layer_parameters = leaf_layer_params;
    metrics::histogram!(
                "prover_fri.witness_generation.prepare_job_time",
                started_at.elapsed(),
                "aggregation_round" => format!("{:?}", AggregationRound::Scheduler),
    );

    SchedulerWitnessGeneratorJob {
        block_number: l1_batch_number,
        scheduler_witness,
        node_vk,
    }
}
