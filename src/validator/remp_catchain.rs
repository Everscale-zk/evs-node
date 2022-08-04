use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fmt::Display;
use std::sync::Arc;
use std::time::SystemTime;
use ever_crypto::KeyId;
use sha2::{Digest, Sha256};
use arc_swap::ArcSwapOption;
use crossbeam_channel::{Sender, Receiver, unbounded};
use ton_api::IntoBoxed;
use ton_api::ton::ton_node::RempMessageStatus;
use ton_block::{ShardIdent, ValidatorDescr};
use ton_types::{Result, UInt256, error, fail};
use catchain::{BlockPayloadPtr, BlockPtr, Catchain, CatchainFactory, CatchainListener, CatchainNode, CatchainOverlayManagerPtr, CatchainPtr, ExternalQueryResponseCallback, PrivateKey, PublicKey, PublicKeyHash};
use crate::engine_traits::EngineOperations;
use crate::validator::catchain_overlay::CatchainOverlayManagerImpl;
use crate::validator::message_cache::RmqMessage;
use crate::validator::mutex_wrapper::MutexWrapper;
use crate::validator::remp_manager::RempManager;
use crate::validator::validator_utils::{compute_validator_list_id, get_validator_key_idx, validatordescr_to_catchain_node};

pub struct RempCatchainInfo {
    rt: tokio::runtime::Handle,
    engine: Arc<dyn EngineOperations>,
    remp_manager: Arc<RempManager>,

    nodes: Vec<CatchainNode>,
    node_list_id: UInt256,
    pub queue_id: UInt256,
    pub local_idx: u32,
    pub local_key_id: UInt256,
    pub master_cc_seqno: u32,
    pub shard: ShardIdent,
    pub catchain_ptr: ArcSwapOption<CatchainPtr>,

    pending_messages_queue_receiver: Receiver<Arc<RmqMessage>>,
    pub pending_messages_queue_sender: Sender<Arc<RmqMessage>>,

    pub rmq_catchain_receiver: Receiver<(RmqMessage, RempMessageStatus)>,
    rmq_catchain_sender: Sender<(RmqMessage, RempMessageStatus)>
}

impl RempCatchainInfo {
    pub fn create(
        rt: tokio::runtime::Handle,
        engine: Arc<dyn EngineOperations>,
        remp_manager: Arc<RempManager>,
        catchain_seqno: u32,
        master_cc_seqno: u32,
        curr: &Vec<ValidatorDescr>,
        next: &Vec<ValidatorDescr>,
        local: &PublicKey,
        shard: ShardIdent
    ) -> Result<Self> {
        let mut nodes: Vec<CatchainNode> = curr.iter().map(validatordescr_to_catchain_node).collect();
        let mut nodes_vdescr = curr.clone();

        let mut adnl_hash: HashSet<Arc<KeyId>> = HashSet::new();
        for nn in nodes.iter() {
            adnl_hash.insert(nn.adnl_id.clone());
        }

        for next_nn in next.iter() {
            let next_cn = validatordescr_to_catchain_node(next_nn);
            if !adnl_hash.contains(&next_cn.adnl_id) {
                nodes.push(next_cn);
                nodes_vdescr.push(next_nn.clone());
            }
        }

        let local_key_id = local.id().data().into();
        let local_idx = get_validator_key_idx(local, &nodes)?;
        let queue_id = RempCatchainInfo::compute_id(catchain_seqno, curr, next);
        let node_list_id = compute_validator_list_id(&nodes_vdescr, Some((&shard, catchain_seqno))).unwrap();

        let node_list_string: String = nodes.iter().map(
            |x| format!("{:x}/{:x} ",
                        UInt256::from(x.adnl_id.data()),
                        UInt256::from(x.public_key.id().data()))
        ).collect();

        log::trace!(target: "remp", "New message queue: id {:x}, nodes: {}, local_idx: {}, master_cc: {}",
            queue_id,
            node_list_string,
            local_idx,
            master_cc_seqno
        );

        let (recv_messages_queue_sender, recv_messages_queue_receiver) = unbounded();
        let (pending_messages_queue_sender, pending_messages_queue_receiver) = unbounded();

        return Ok(Self {
            rt,
            engine,
            queue_id,
            nodes,
            local_idx,
            local_key_id,
            node_list_id,
            master_cc_seqno,
            shard,
            catchain_ptr: ArcSwapOption::new(None),
            pending_messages_queue_sender, pending_messages_queue_receiver,
            rmq_catchain_sender: recv_messages_queue_sender,
            rmq_catchain_receiver: recv_messages_queue_receiver,
            /*
                        pending_messages_queue_sender, pending_messages_queue_receiver,
                        rmq_catchain_sender: recv_messages_queue_sender,
                        rmq_catchain_receiver: recv_messages_queue_receiver,
                        rmq_catchain,
             */
            remp_manager
        });
    }

    pub fn compute_id(catchain_seqno: u32, current: &Vec<ValidatorDescr>, next: &Vec<ValidatorDescr>) -> UInt256 {
        let mut hasher = Sha256::new();
        hasher.update(catchain_seqno.to_be_bytes());
        for x in current {
            hasher.update(x.compute_node_id_short().as_slice());
        }
        for x in next {
            hasher.update(x.compute_node_id_short().as_slice());
        }
        From::<[u8; 32]>::from(hasher.finalize().into())
    }

    pub fn is_same_catchain(&self, other: Arc<RempCatchainInfo>) -> bool {
        let general_eq = self.queue_id == other.queue_id &&
            self.nodes.len() == other.nodes.len() &&
            self.local_idx == other.local_idx &&
            self.local_key_id == other.local_key_id &&
            self.node_list_id == other.node_list_id &&
            self.master_cc_seqno == other.master_cc_seqno &&
            self.shard == other.shard;

        general_eq
    }

    pub async fn start(self: Arc<RempCatchainInfo>, local_key: PrivateKey) -> Result<CatchainPtr> {
        let overlay_manager: CatchainOverlayManagerPtr =
            Arc::new(CatchainOverlayManagerImpl::new(self.engine.validator_network(), self.node_list_id));
        let db_root = format!("{}/rmq", self.engine.db_root_dir()?);
        let db_suffix = "".to_string();
        let allow_unsafe_self_blocks_resync = false;

        let message_listener = Arc::downgrade(&self);

        log::info!(target: "remp", "Do starting RMQ Catchain session {} list_id={} with nodes {:?}",
            self,
            self.node_list_id.to_hex_string(),
            self.nodes.iter().map(|x| x.adnl_id.to_string()).collect::<Vec<String>>()
        );

        let rmq_local_key = if let Some(rmq_local_key) = self.engine.set_validator_list(self.node_list_id, &self.nodes).await? {
            rmq_local_key.clone()
        } else {
            return Err(failure::err_msg(format!("Cannot add RMQ validator list {}", self.node_list_id.to_hex_string())));
        };

        let rmq_catchain_options = self.remp_manager.options.get_catchain_options().ok_or_else(
            || error!("RMQ {}: cannot get REMP catchain options, start is impossible", self)
        )?;

        let catchain_session_ptr = CatchainFactory::create_catchain(
            &rmq_catchain_options,
            &self.queue_id,
            &self.nodes,
            &local_key, //&rmq_local_key,
            db_root,
            db_suffix,
            allow_unsafe_self_blocks_resync,
            overlay_manager,
            message_listener
        );

        log::info!(target: "remp", "RMQ session {} started: list_id: {:x}, rmq_local_key = {}, local_key = {}",
            self, self.node_list_id, rmq_local_key.id(), local_key.id());
        Ok(catchain_session_ptr)
    }

    pub async fn stop(&self, session_opt: Option<Arc<CatchainPtr>>) -> Result<()> {
        log::info!(target: "remp", "Do stopping RMQ Catchain session {} list_id={:x}", self, self.node_list_id);
        match session_opt {
            Some(session) => session.stop(true),
            _ => log::trace!(target: "remp", "Queue {} is destroyed, but not started", self)
        };
        // TODO: check whether this removal is not ahead-of-time
        log::trace!(target: "remp", "RMQ session {}, removing validator list, list_id {:x}", self, self.node_list_id);
        self.engine.remove_validator_list(self.node_list_id).await?;
        log::trace!(target: "remp", "RMQ session {} stopped", self);
        Ok(())
    }

    pub fn set_session(&self, catchain: CatchainPtr) {
        self.catchain_ptr.store(Some(Arc::new(catchain.clone())));
    }

    pub fn get_session(&self) -> Option<Arc<CatchainPtr>> {
        self.catchain_ptr.load()
    }

    fn unpack_payload(&self, payload: &BlockPayloadPtr, source_idx: u32) {
        log::trace!(target: "remp", "RMQ {} unpacking message {:?} from {}", self, payload.data().0, source_idx);

        let pld: Result<::ton_api::ton::validator_session::BlockUpdate> =
            catchain::utils::deserialize_tl_boxed_object(payload.data());

        match pld {
            Ok(::ton_api::ton::validator_session::BlockUpdate::ValidatorSession_BlockUpdate(pld)) => {
                let mut total = 0;
                let mut ignored = 0;
                for msgbx in pld.actions.0.iter() {
                    match msgbx {
                        ::ton_api::ton::validator_session::round::Message::ValidatorSession_Message_Commit(msg) => {
                            match RmqMessage::deserialize(&msg.signature, self.master_cc_seqno) {
                                Ok((decoded_msg, decoded_msg_status)) => {
                                    total += 1;
                                    if decoded_msg_status != RempMessageStatus::TonNode_RempNew {
                                        log::trace!(target: "remp",
                                            "Point 4. RMQ {} received message: {:?}, decoded {}, ignoring",
                                            self, msg.signature.0, decoded_msg
                                        );
                                        ignored += 1;
                                    }
                                    else {
                                        log::trace!(target: "remp",
                                            "Point 4. Message received from RMQ {}: {:?}, decoded {}, put to rmq_catchain queue",
                                            self, msg.signature.0, decoded_msg //catchain.received_messages.len()
                                        );
                                        if let Err(e) = self.rmq_catchain_sender.send((decoded_msg.clone(), decoded_msg_status)) {
                                            log::error!(target: "remp", "Point 4. Cannot put message {} from RMQ {} to queue: {}",
                                                decoded_msg, self, e
                                            )
                                        }
                                    }
                                }
                                Err(e) => log::error!(target: "remp", "Cannot deserialize message from RMQ {} {:?}: {}",
                                        self, msg.signature.0, e
                                )
                            }
                        },
                        _ => log::error!(target: "remp", "Point 4. RMQ {}: only Commit messages are expected", self)
                    }
                };
                #[cfg(feature = "telemetry")] {
                    self.engine.remp_core_telemetry().got_from_catchain(&self.shard, total, ignored);
                    self.engine.remp_core_telemetry().in_channel_to_rmq(&self.shard, self.rmq_catchain_sender.len());
                }

            }
            Err(e) => log::error!(target: "remp", "Cannot deserialize RMQ {} message: {}", self, e)
        }
    }
}

impl fmt::Display for RempCatchainInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}*{:x}*{}", self.local_idx, self.queue_id, self.shard)
    }
}

impl CatchainListener for RempCatchainInfo {
    fn preprocess_block(&self, block: BlockPtr) {
        let data = block.get_payload();
        log::trace!(target: "remp", "Preprocessing RMQ {} Message {:?} from {}",
            self, data.data().0, block.get_source_id()
        );
        self.unpack_payload(data, block.get_source_id());
    }

    fn process_blocks(&self, blocks: Vec<BlockPtr>) {
        log::trace!(target: "remp", "Processing RMQ {}: new external messages, len = {}", self, blocks.len());
        //if blocks.len() > 0 {
        //    for x in blocks {
        //        self.unpack_payload(x.get_payload(), x.get_source_id());
        //    }
        //}

        let mut msg_vect: Vec<::ton_api::ton::validator_session::round::Message> = Vec::new();
        while let Ok(msg) = self.pending_messages_queue_receiver.try_recv() {
            let msg_body = ::ton_api::ton::validator_session::round::validator_session::message::message::Commit {
                round: 0,
                candidate: Default::default(),
                signature: msg.serialize(RempMessageStatus::TonNode_RempNew).unwrap()
            }.into_boxed();
            log::trace!(target: "remp", "Point 3. RMQ {} sending message: {:?}, decoded {:?}",
                self, msg_body, msg
            );

            msg_vect.push(msg_body);
        };

        #[cfg(feature = "telemetry")]
            let sent_to_catchain = msg_vect.len();

        let payload = ::ton_api::ton::validator_session::blockupdate::BlockUpdate {
            ts: 0, //ts as i64,
            actions: msg_vect.into(),
            state: 0 //real_state_hash as i32,
        }.into_boxed();
        let serialized_payload = catchain::utils::serialize_tl_boxed_object!(&payload);

        match &self.get_session() {
            Some(ctchn) => {
                ctchn.processed_block(
                    catchain::CatchainFactory::create_block_payload(
                        serialized_payload.clone(),
                    ), false, false);
                log::trace!(target: "remp", "Point 3. RMQ {} sent message: {:?}",
                    self, serialized_payload
                );
                #[cfg(feature = "telemetry")]
                self.engine.remp_core_telemetry().sent_to_catchain(&self.shard, sent_to_catchain);
            },
            None => log::error!("RMQ: Catchain session is not initialized!")
        }
    }

    fn finished_processing(&self) {
        log::trace!(target: "remp", "MessageQueue {} finished processing", self)
    }

    fn started(&self) {
        log::trace!(target: "remp", "MessageQueue {} started", self)
    }

    fn process_broadcast(&self, _source_id: PublicKeyHash, _data: BlockPayloadPtr) {
        log::trace!(target: "remp", "MessageQueue {} process broadcast", self)
    }

    fn process_query(&self, source_id: PublicKeyHash, data: BlockPayloadPtr, _callback: ExternalQueryResponseCallback) {
        let data = data.data();
        log::trace!(target: "remp", "Processing RMQ {} Query {:?} from {}", self, data.0.as_slice(), source_id);
    }

    fn set_time(&self, _timestamp: SystemTime) {
        log::trace!(target: "remp", "MessageQueue {} set time", self)
    }
}

#[derive(Debug,PartialEq,Eq)]
enum RempCatchainStatus {
    Created, Starting, Active, Stopping
}

struct RempCatchainWrapper {
    info: Arc<RempCatchainInfo>,
    status: RempCatchainStatus,
    count: u32
}

impl RempCatchainWrapper {
    pub fn create(info: Arc<RempCatchainInfo>) -> Self {
        RempCatchainWrapper {
            info,
            status: RempCatchainStatus::Created,
            count: 1
        }
    }

    pub fn attach(&mut self) {
        self.count += 1;
    }

    pub fn detach(&mut self) -> bool {
        self.count -= 1;
        self.count <= 0
    }

    pub fn set_status(&mut self, status: RempCatchainStatus) -> Result<()> {
        self.status = status;
        Ok(())
    }

    pub fn set_active(&mut self) -> Result<()> {
        if self.status == RempCatchainStatus::Starting {
            self.status = RempCatchainStatus::Active;
            Ok(())
        }
        else {
            fail!("RempCatchainWrapper {}: cannot set active, incompatible current status", self)
        }
    }
}

impl Display for RempCatchainWrapper {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({:?})", self.info, self.status)
    }
}

pub struct RempCatchainStore {
    catchains: MutexWrapper<HashMap<UInt256, RempCatchainWrapper>>
}

impl RempCatchainStore {
    pub fn new() -> Self {
        RempCatchainStore { catchains: MutexWrapper::new(HashMap::new(), "CatchainStore".to_string()) }
    }

    pub async fn create_catchain(&self, remp_catchain_info: Arc<RempCatchainInfo>) -> Result<()> {
        self.catchains.execute_sync(|x| {
            if let Some(rcw) = x.get_mut(&remp_catchain_info.queue_id) {
                if rcw.info.is_same_catchain(remp_catchain_info.clone()) {
                    rcw.attach();
                    Ok(())
                } else {
                    fail!("REMP Catchain Store: adding different catchain {} (to {}) for same id {:x}",
                        remp_catchain_info, rcw.info, remp_catchain_info.queue_id
                    )
                }
            }
            else {
                x.insert(remp_catchain_info.queue_id.clone(), RempCatchainWrapper::create(remp_catchain_info));
                Ok(())
            }
        }).await?;

        Ok(())
    }

    pub async fn start_catchain(&self, session_id: UInt256, local_key: PrivateKey) -> Result<()> {
        log::trace!(target: "remp", "Starting REMP catchain {:x}", session_id);
        let catchain_to_start = self.catchains.execute_sync(|x| {
            match x.get_mut(&session_id) {
                Some(cc) => {
                    match cc.status {
                        RempCatchainStatus::Created => {
                            cc.status = RempCatchainStatus::Starting;
                            Ok(Some(cc.info.clone()))
                        },
                        RempCatchainStatus::Starting | RempCatchainStatus::Stopping =>
                            fail!("REMP Catchain session {} is being started/stopped already", cc),
                        RempCatchainStatus::Active => {
                            log::trace!(target: "remp", "REMP catchain {:x} is already started", session_id);
                            return Ok(None)
                        }
                    }
                }
                None => fail!("REMP Catchain session {:x} start impossible -- not found", session_id)
            }
        }).await?;

        if let Some(cc_info) = &catchain_to_start {
            log::trace!(target: "remp", "Actually starting REMP catchain {:x}/{}",
                cc_info.queue_id, cc_info.shard
            );
            let catchain_ptr = cc_info.clone().start(local_key).await?;
            cc_info.clone().set_session(catchain_ptr);
            self.catchains.execute_sync(|x| {
                match x.get_mut(&session_id) {
                    Some(cc) => cc.set_active(),
                    None => Ok(())
                }
            }).await
        }
        else { Ok(()) }
    }

    pub async fn stop_catchain(&self, session_id: &UInt256) -> Result<()> {
        log::trace!(target: "remp", "Stopping REMP catchain {:x}", session_id);
        let to_remove = self.catchains.execute_sync(|x| {
            match x.get_mut(session_id) {
                Some(catchain) => {
                    if catchain.status != RempCatchainStatus::Active {
                        fail!("REMP Catchain session {} stopping impossible -- session should be in 'Active' state", catchain)
                    }
                    else if catchain.detach() {
                        catchain.status = RempCatchainStatus::Stopping;
                        Ok(Some((catchain.info.clone(), catchain.info.catchain_ptr.load().clone())))
                    }
                    else { Ok(None) }
                },
                None => fail!("REMP Catchain session {:x} not found!", session_id)
            }
        }).await?;

        if let Some((info,session)) = to_remove {
            info.stop(session).await?;
            self.catchains.execute_sync(|x| x.remove(session_id)).await;
            Ok(())
        }
        else {
            fail!("REMP catchain session already removed: {:x}", session_id)
        }
    }

    pub async fn get_catchain_session(&self, session_id: &UInt256) -> Option<Arc<CatchainPtr>> {
        self.catchains.execute_sync(|x| {
            x.get(session_id).map(|rcw| rcw.info.get_session()).flatten()
        }).await
    }
}