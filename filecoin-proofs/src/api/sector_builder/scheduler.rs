use crate::api::internal;
use crate::api::internal::PoStInput;
use crate::api::internal::PoStInputPart;
use crate::api::internal::PoStOutput;
use crate::api::sector_builder::errors::err_piecenotfound;
use crate::api::sector_builder::errors::err_unrecov;
use crate::api::sector_builder::helpers::add_piece::add_piece;
use crate::api::sector_builder::helpers::get_seal_status::get_seal_status;
use crate::api::sector_builder::helpers::get_sectors_ready_for_sealing::get_sectors_ready_for_sealing;
use crate::api::sector_builder::helpers::snapshots::load_snapshot;
use crate::api::sector_builder::helpers::snapshots::make_snapshot;
use crate::api::sector_builder::helpers::snapshots::persist_snapshot;
use crate::api::sector_builder::metadata::SealStatus;
use crate::api::sector_builder::metadata::SealedSectorMetadata;
use crate::api::sector_builder::metadata::StagedSectorMetadata;
use crate::api::sector_builder::sealer::SealerInput;
use crate::api::sector_builder::state::SectorBuilderState;
use crate::api::sector_builder::state::StagedState;
use crate::api::sector_builder::SectorId;
use crate::api::sector_builder::WrappedKeyValueStore;
use crate::api::sector_builder::WrappedSectorStore;
use crate::error::ExpectWithBacktrace;
use crate::error::Result;
use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;

const FATAL_NOLOAD: &str = "could not load snapshot";
const FATAL_NORECV: &str = "could not receive task";
const FATAL_NOSEND: &str = "could not send";
const FATAL_SECMAP: &str = "insert failed";
const FATAL_SNPSHT: &str = "could not snapshot";
const FATAL_SLRSND: &str = "could not send to sealer";
const FATAL_HUNGUP: &str = "could not send to ret channel";
const FATAL_NOSECT: &str = "could not find sector";

pub struct Scheduler {
    pub thread: Option<thread::JoinHandle<()>>,
}

#[derive(Debug)]
pub enum Request {
    AddPiece(String, Vec<u8>, mpsc::SyncSender<Result<SectorId>>),
    GetSealedSectors(mpsc::SyncSender<Result<Vec<SealedSectorMetadata>>>),
    GetStagedSectors(mpsc::SyncSender<Result<Vec<StagedSectorMetadata>>>),
    GetSealStatus(SectorId, mpsc::SyncSender<Result<SealStatus>>),
    GeneratePoSt(
        Vec<[u8; 32]>,
        [u8; 32],
        mpsc::SyncSender<Result<PoStOutput>>,
    ),
    RetrievePiece(String, mpsc::SyncSender<Result<Vec<u8>>>),
    SealAllStagedSectors(mpsc::SyncSender<Result<()>>),
    GetMaxUserBytesPerStagedSector(mpsc::SyncSender<u64>),
    HandleSealResult(SectorId, Box<Result<SealedSectorMetadata>>),
    Shutdown,
}

impl Scheduler {
    pub fn start_with_metadata(
        scheduler_input_rx: mpsc::Receiver<Request>,
        scheduler_input_tx: mpsc::SyncSender<Request>,
        sealer_input_tx: mpsc::Sender<SealerInput>,
        kv_store: Arc<WrappedKeyValueStore>,
        sector_store: Arc<WrappedSectorStore>,
        last_committed_sector_id: SectorId,
        max_num_staged_sectors: u8,
        prover_id: [u8; 31],
    ) -> Scheduler {
        let thread = thread::spawn(move || {
            // Build the scheduler's initial state. If available, we
            // reconstitute this state from persisted metadata. If not, we
            // create it from scratch.
            let state = {
                let loaded = load_snapshot(&kv_store, &prover_id)
                    .expects(FATAL_NOLOAD)
                    .map(|x| x.into());

                loaded.unwrap_or_else(|| SectorBuilderState {
                    prover_id,
                    staged: StagedState {
                        sector_id_nonce: last_committed_sector_id,
                        sectors: Default::default(),
                    },
                    sealed: Default::default(),
                })
            };

            let max_user_bytes_per_staged_sector =
                sector_store.inner.config().max_unsealed_bytes_per_sector();

            let mut m = SectorMetadataManager {
                kv_store,
                sector_store,
                state,
                sealer_input_tx,
                scheduler_input_tx: scheduler_input_tx.clone(),
                max_num_staged_sectors,
                max_user_bytes_per_staged_sector,
            };

            loop {
                let task = scheduler_input_rx.recv().expects(FATAL_NORECV);

                // Dispatch to the appropriate task-handler.
                match task {
                    Request::AddPiece(key, bytes, tx) => {
                        tx.send(m.add_piece(key, &bytes)).expects(FATAL_NOSEND);
                    }
                    Request::GetSealStatus(sector_id, tx) => {
                        tx.send(m.get_seal_status(sector_id)).expects(FATAL_NOSEND);
                    }
                    Request::RetrievePiece(piece_key, tx) => m.retrieve_piece(piece_key, tx),
                    Request::GetSealedSectors(tx) => {
                        tx.send(m.get_sealed_sectors()).expects(FATAL_NOSEND);
                    }
                    Request::GetStagedSectors(tx) => {
                        tx.send(m.get_staged_sectors()).expect(FATAL_NOSEND);
                    }
                    Request::GetMaxUserBytesPerStagedSector(tx) => {
                        tx.send(m.max_user_bytes()).expects(FATAL_NOSEND);
                    }
                    Request::SealAllStagedSectors(tx) => {
                        tx.send(m.seal_all_staged_sectors()).expects(FATAL_NOSEND);
                    }
                    Request::HandleSealResult(sector_id, result) => {
                        m.handle_seal_result(sector_id, *result);
                    }
                    Request::GeneratePoSt(comm_rs, chg_seed, tx) => {
                        m.generate_post(&comm_rs, &chg_seed, tx)
                    }
                    Request::Shutdown => break,
                }
            }
        });

        Scheduler {
            thread: Some(thread),
        }
    }
}

// The SectorBuilderStateManager is the owner of all sector-related metadata.
// It dispatches expensive operations (e.g. unseal and seal) to the sealer
// worker-threads. Other, inexpensive work (or work which needs to be performed
// serially) is handled by the SectorBuilderStateManager itself.
pub struct SectorMetadataManager {
    kv_store: Arc<WrappedKeyValueStore>,
    sector_store: Arc<WrappedSectorStore>,
    state: SectorBuilderState,
    sealer_input_tx: mpsc::Sender<SealerInput>,
    scheduler_input_tx: mpsc::SyncSender<Request>,
    max_num_staged_sectors: u8,
    max_user_bytes_per_staged_sector: u64,
}

impl SectorMetadataManager {
    pub fn generate_post(
        &self,
        comm_rs: &[[u8; 32]],
        challenge_seed: &[u8; 32],
        return_channel: mpsc::SyncSender<Result<PoStOutput>>,
    ) {
        // reduce our sealed sector state-map to a mapping of comm_r to sealed
        // sector access (AKA path to sealed sector file)
        let comm_r_to_sector_access: HashMap<[u8; 32], String> = self
            .state
            .sealed
            .sectors
            .values()
            .fold(HashMap::new(), |mut acc, item| {
                let v = item.sector_access.clone();
                let k = item.comm_r;
                acc.entry(k).or_insert(v);
                acc
            });

        let mut input_parts: Vec<PoStInputPart> = Default::default();

        // eject from this loop with an error if we've been provided a comm_r
        // which does not correspond to any sealed sector metadata
        for comm_r in comm_rs {
            input_parts.push(PoStInputPart {
                sealed_sector_access: comm_r_to_sector_access.get(comm_r).cloned(),
                comm_r: *comm_r,
            });
        }

        let output = internal::generate_post(PoStInput {
            challenge_seed: *challenge_seed,
            input_parts,
        });

        // TODO: Where should this work be scheduled? New worker type?
        return_channel.send(output).expects(FATAL_HUNGUP);
    }

    // Unseals the sector containing the referenced piece and returns its
    // bytes. Produces an error if this sector builder does not have a sealed
    // sector containing the referenced piece.
    pub fn retrieve_piece(
        &self,
        piece_key: String,
        return_channel: mpsc::SyncSender<Result<Vec<u8>>>,
    ) {
        let opt_sealed_sector = self.state.sealed.sectors.values().find(|sector| {
            sector
                .pieces
                .iter()
                .any(|piece| piece.piece_key == piece_key)
        });

        if let Some(sealed_sector) = opt_sealed_sector {
            let sealed_sector = Box::new(sealed_sector.clone());
            let task = SealerInput::Unseal(piece_key, sealed_sector, return_channel);

            self.sealer_input_tx
                .clone()
                .send(task)
                .expects(FATAL_SLRSND);
        } else {
            return_channel
                .send(Err(err_piecenotfound(piece_key.to_string()).into()))
                .expects(FATAL_HUNGUP);
        }
    }

    // Returns sealing status for the sector with specified id. If no sealed or
    // staged sector exists with the provided id, produce an error.
    pub fn get_seal_status(&self, sector_id: SectorId) -> Result<SealStatus> {
        get_seal_status(&self.state.staged, &self.state.sealed, sector_id)
    }

    // Write the piece to storage, obtaining the sector id with which the
    // piece-bytes are now associated.
    pub fn add_piece(&mut self, piece_key: String, piece_bytes: &[u8]) -> Result<u64> {
        let destination_sector_id = add_piece(
            &self.sector_store,
            &mut self.state.staged,
            piece_key,
            piece_bytes,
        )?;

        self.check_and_schedule(false)?;
        self.checkpoint()?;

        Ok(destination_sector_id)
    }

    // For demo purposes. Schedules sealing of all staged sectors.
    pub fn seal_all_staged_sectors(&mut self) -> Result<()> {
        self.check_and_schedule(true)?;
        self.checkpoint()
    }

    // Produces a vector containing metadata for all sealed sectors that this
    // SectorBuilder knows about.
    pub fn get_sealed_sectors(&self) -> Result<Vec<SealedSectorMetadata>> {
        Ok(self.state.sealed.sectors.values().cloned().collect())
    }

    // Produces a vector containing metadata for all staged sectors that this
    // SectorBuilder knows about.
    pub fn get_staged_sectors(&self) -> Result<Vec<StagedSectorMetadata>> {
        Ok(self.state.staged.sectors.values().cloned().collect())
    }

    // Returns the number of user-provided bytes that will fit into a staged
    // sector.
    pub fn max_user_bytes(&self) -> u64 {
        self.max_user_bytes_per_staged_sector
    }

    // Update metadata to reflect the sealing results.
    pub fn handle_seal_result(
        &mut self,
        sector_id: SectorId,
        result: Result<SealedSectorMetadata>,
    ) {
        // scope exists to end the mutable borrow of self so that we can
        // checkpoint
        {
            let staged_state = &mut self.state.staged;
            let sealed_state = &mut self.state.sealed;

            if result.is_err() {
                if let Some(staged_sector) = staged_state.sectors.get_mut(&sector_id) {
                    staged_sector.seal_status =
                        SealStatus::Failed(format!("{}", err_unrecov(result.unwrap_err())));
                };
            } else {
                // Remove the staged sector from the state map.
                let _ = staged_state.sectors.remove(&sector_id);

                // Insert the newly-sealed sector into the other state map.
                let sealed_sector = result.expects(FATAL_SECMAP);

                sealed_state.sectors.insert(sector_id, sealed_sector);
            }
        }

        self.checkpoint().expects(FATAL_SNPSHT);
    }

    // Check for sectors which should no longer receive new user piece-bytes and
    // schedule them for sealing.
    fn check_and_schedule(&mut self, seal_all_staged_sectors: bool) -> Result<()> {
        let staged_state = &mut self.state.staged;

        let to_be_sealed = get_sectors_ready_for_sealing(
            staged_state,
            self.max_user_bytes_per_staged_sector,
            self.max_num_staged_sectors,
            seal_all_staged_sectors,
        );

        // Mark the to-be-sealed sectors as no longer accepting data and then
        // schedule sealing.
        for sector_id in to_be_sealed {
            let mut sector = staged_state
                .sectors
                .get_mut(&sector_id)
                .expects(FATAL_NOSECT);
            sector.seal_status = SealStatus::Sealing;

            self.sealer_input_tx
                .clone()
                .send(SealerInput::Seal(
                    sector.clone(),
                    self.scheduler_input_tx.clone(),
                ))
                .expects(FATAL_SLRSND);
        }

        Ok(())
    }

    // Create and persist metadata snapshot.
    fn checkpoint(&self) -> Result<()> {
        let snapshot = make_snapshot(
            &self.state.prover_id,
            &self.state.staged,
            &self.state.sealed,
        );
        persist_snapshot(&self.kv_store, &snapshot)?;

        Ok(())
    }
}
