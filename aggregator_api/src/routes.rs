use crate::{
    models::{
        AggregatorApiConfig, AggregatorRole, DeleteTaskprovPeerAggregatorReq, GetTaskIdsResp,
        GetTaskMetricsResp, GetTaskUploadMetricsResp, GlobalHpkeConfigResp,
        PatchGlobalHpkeConfigReq, PostTaskReq, PostTaskprovPeerAggregatorReq,
        PutGlobalHpkeConfigReq, SupportedVdaf, TaskResp, TaskprovPeerAggregatorResp,
    },
    Config, Error,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use janus_aggregator_core::{
    datastore::{self, Datastore},
    task::{AggregatorTask, AggregatorTaskParameters},
    taskprov::PeerAggregator,
    SecretBytes,
};
use janus_core::{
    auth_tokens::AuthenticationTokenHash, hpke::generate_hpke_config_and_private_key, time::Clock,
};
use janus_messages::HpkeConfigId;
use janus_messages::{
    query_type::Code as SupportedQueryType, Duration, HpkeAeadId, HpkeKdfId, HpkeKemId, Role,
    TaskId,
};
use querystring::querify;
use rand::random;
use ring::digest::{digest, SHA256};
use std::{ops::Deref, str::FromStr, sync::Arc, unreachable};
use trillium::{async_trait, Conn, Status};
use trillium_api::{Json, State, TryFromConn};
use trillium_router::RouterConnExt;

type Store<C> = State<Arc<Datastore<C>>>;

pub(super) async fn get_config(State(config): State<Arc<Config>>) -> Json<AggregatorApiConfig> {
    Json(AggregatorApiConfig {
        protocol: "DAP-07",
        dap_url: config.public_dap_url.clone(),
        role: AggregatorRole::Either,
        vdafs: vec![
            SupportedVdaf::Prio3Count,
            SupportedVdaf::Prio3Sum,
            SupportedVdaf::Prio3Histogram,
            SupportedVdaf::Prio3SumVec,
        ],
        query_types: vec![
            SupportedQueryType::TimeInterval,
            SupportedQueryType::FixedSize,
        ],
        // Unconditionally indicate to divviup-api that we support collector auth token hashes
        features: &["TokenHash"],
    })
}

pub(crate) struct LowerBound(Option<TaskId>);
#[async_trait]
impl TryFromConn for LowerBound {
    type Error = Error;
    async fn try_from_conn(conn: &mut Conn) -> Result<Self, Self::Error> {
        querify(conn.querystring())
            .into_iter()
            .find(|&(k, _)| k == "pagination_token")
            .map(|(_, v)| TaskId::from_str(v))
            .transpose()
            .map(Self)
            .map_err(|err| Error::BadRequest(format!("Couldn't parse pagination_token: {:?}", err)))
    }
}

pub(super) async fn get_task_ids<C: Clock>(
    (LowerBound(lower_bound), ds): (LowerBound, Store<C>),
) -> Result<Json<GetTaskIdsResp>, Error> {
    let task_ids = ds
        .run_tx("get_task_ids", |tx| {
            Box::pin(async move { tx.get_task_ids(lower_bound).await })
        })
        .await?;
    let pagination_token = task_ids.last().cloned();

    Ok(Json(GetTaskIdsResp {
        task_ids,
        pagination_token,
    }))
}

pub(super) async fn post_task<C: Clock>(
    (ds, Json(req)): (Store<C>, Json<PostTaskReq>),
) -> Result<Json<TaskResp>, Error> {
    if !matches!(req.role, Role::Leader | Role::Helper) {
        return Err(Error::BadRequest(format!("invalid role {}", req.role)));
    }

    let vdaf_verify_key_bytes = URL_SAFE_NO_PAD
        .decode(&req.vdaf_verify_key)
        .map_err(|err| {
            Error::BadRequest(format!("Invalid base64 value for vdaf_verify_key: {err}"))
        })?;
    if vdaf_verify_key_bytes.len() != req.vdaf.verify_key_length() {
        return Err(Error::BadRequest(format!(
            "Wrong VDAF verify key length, expected {}, got {}",
            req.vdaf.verify_key_length(),
            vdaf_verify_key_bytes.len()
        )));
    }

    // DAP recommends deriving the task ID from the VDAF verify key. We deterministically obtain a
    // 32 byte task ID by taking SHA-256(VDAF verify key).
    // https://datatracker.ietf.org/doc/html/draft-ietf-ppm-dap-04#name-verification-key-requiremen
    let task_id = TaskId::try_from(digest(&SHA256, &vdaf_verify_key_bytes).as_ref())
        .map_err(|err| Error::Internal(err.to_string()))?;

    let vdaf_verify_key = SecretBytes::new(vdaf_verify_key_bytes);

    let (aggregator_auth_token, aggregator_parameters) = match req.role {
        Role::Leader => {
            let aggregator_auth_token = req.aggregator_auth_token.ok_or_else(|| {
                Error::BadRequest(
                    "aggregator acting in leader role must be provided an aggregator auth token"
                        .to_string(),
                )
            })?;
            let collector_auth_token_hash = req.collector_auth_token_hash.ok_or_else(|| {
                Error::BadRequest(
                    "aggregator acting in leader role must be provided a collector auth token hash"
                        .to_string(),
                )
            })?;
            (
                None,
                AggregatorTaskParameters::Leader {
                    aggregator_auth_token,
                    collector_auth_token_hash,
                    collector_hpke_config: req.collector_hpke_config,
                },
            )
        }

        Role::Helper => {
            if req.aggregator_auth_token.is_some() {
                return Err(Error::BadRequest(
                    "aggregator acting in helper role cannot be given an aggregator auth token"
                        .to_string(),
                ));
            }

            let aggregator_auth_token = random();
            let aggregator_auth_token_hash = AuthenticationTokenHash::from(&aggregator_auth_token);
            (
                Some(aggregator_auth_token),
                AggregatorTaskParameters::Helper {
                    aggregator_auth_token_hash,
                    collector_hpke_config: req.collector_hpke_config,
                },
            )
        }

        _ => unreachable!(),
    };

    let task = Arc::new(
        AggregatorTask::new(
            task_id,
            /* peer_aggregator_endpoint */ req.peer_aggregator_endpoint,
            /* query_type */ req.query_type,
            /* vdaf */ req.vdaf,
            vdaf_verify_key,
            /* max_batch_query_count */ req.max_batch_query_count,
            /* task_expiration */ req.task_expiration,
            /* report_expiry_age */
            Some(Duration::from_seconds(3600 * 24 * 7 * 2)), // 2 weeks
            /* min_batch_size */ req.min_batch_size,
            /* time_precision */ req.time_precision,
            /* tolerable_clock_skew */
            Duration::from_seconds(60), // 1 minute,
            // hpke_keys
            // Unwrap safety: we always use a supported KEM.
            [generate_hpke_config_and_private_key(
                random(),
                HpkeKemId::X25519HkdfSha256,
                HpkeKdfId::HkdfSha256,
                HpkeAeadId::Aes128Gcm,
            )
            .unwrap()],
            aggregator_parameters,
        )
        .map_err(|err| Error::BadRequest(format!("Error constructing task: {err}")))?,
    );

    ds.run_tx("post_task", |tx| {
        let task = Arc::clone(&task);
        Box::pin(async move {
            if let Some(existing_task) = tx.get_aggregator_task(task.id()).await? {
            // Check whether the existing task in the DB corresponds to the incoming task, ignoring
            // those fields that are randomly generated.
            if existing_task.peer_aggregator_endpoint() == task.peer_aggregator_endpoint()
                && existing_task.query_type() == task.query_type()
                && existing_task.vdaf() == task.vdaf()
                && existing_task.opaque_vdaf_verify_key() == task.opaque_vdaf_verify_key()
                && existing_task.role() == task.role()
                && existing_task.max_batch_query_count() == task.max_batch_query_count()
                && existing_task.task_expiration() == task.task_expiration()
                && existing_task.min_batch_size() == task.min_batch_size()
                && existing_task.time_precision() == task.time_precision()
                && existing_task.collector_hpke_config() == task.collector_hpke_config() {
                    return Ok(())
                }

                let err = Error::Conflict(
                    "task with same VDAF verify key and task ID already exists with different parameters".to_string(),
                );
                return Err(datastore::Error::User(err.into()));
            }

            tx.put_aggregator_task(&task).await
        })
    })
    .await?;

    let mut task_resp =
        TaskResp::try_from(task.as_ref()).map_err(|err| Error::Internal(err.to_string()))?;

    // When creating a new task in the helper, we must put the unhashed aggregator auth token in the
    // response so that divviup-api can later provide it to the leader, but the helper doesn't store
    // the unhashed token and can't later provide it.
    task_resp.aggregator_auth_token = aggregator_auth_token;

    Ok(Json(task_resp))
}

#[derive(Copy, Clone)]
pub(crate) struct TaskIdParam(TaskId);
impl Deref for TaskIdParam {
    type Target = TaskId;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[async_trait]
impl TryFromConn for TaskIdParam {
    type Error = Error;
    async fn try_from_conn(conn: &mut Conn) -> Result<Self, Self::Error> {
        TaskId::from_str(
            conn.param("task_id")
                .ok_or_else(|| Error::Internal("Missing task_id parameter".to_string()))?,
        )
        .map_err(|err| Error::BadRequest(format!("{:?}", err)))
        .map(Self)
    }
}

#[derive(Copy, Clone)]
pub(crate) struct HpkeConfigIdParam(HpkeConfigId);
impl Deref for HpkeConfigIdParam {
    type Target = HpkeConfigId;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
#[async_trait]
impl TryFromConn for HpkeConfigIdParam {
    type Error = Error;
    async fn try_from_conn(conn: &mut Conn) -> Result<Self, Self::Error> {
        Ok(Self(HpkeConfigId::from(
            conn.param("config_id")
                .ok_or_else(|| Error::Internal("Missing config_id parameter".to_string()))?
                .parse::<u8>()
                .map_err(|_| Error::BadRequest("Invalid config_id parameter".to_string()))?,
        )))
    }
}

pub(super) async fn get_task<C: Clock>(
    (task_id, ds): (TaskIdParam, Store<C>),
) -> Result<Json<TaskResp>, Error> {
    let task = ds
        .run_tx("get_task", |tx| {
            Box::pin(async move { tx.get_aggregator_task(&task_id).await })
        })
        .await?
        .ok_or(Error::NotFound)?;

    Ok(Json(
        TaskResp::try_from(&task).map_err(|err| Error::Internal(err.to_string()))?,
    ))
}

pub(super) async fn delete_task<C: Clock>(
    (TaskIdParam(task_id), ds): (TaskIdParam, Store<C>),
) -> Result<Status, Error> {
    match ds
        .run_tx("delete_task", |tx| {
            Box::pin(async move { tx.delete_task(&task_id).await })
        })
        .await
    {
        Ok(_) | Err(datastore::Error::MutationTargetNotFound) => Ok(Status::NoContent),
        Err(err) => Err(err.into()),
    }
}

pub(super) async fn get_task_metrics<C: Clock>(
    (task_id, ds): (TaskIdParam, Store<C>),
) -> Result<Json<GetTaskMetricsResp>, Error> {
    let (reports, report_aggregations) = ds
        .run_tx("get_task_metrics", |tx| {
            Box::pin(async move { tx.get_task_metrics(&task_id).await })
        })
        .await?
        .ok_or(Error::NotFound)?;

    Ok(Json(GetTaskMetricsResp {
        reports,
        report_aggregations,
    }))
}

pub(super) async fn get_task_upload_metrics<C: Clock>(
    (task_id, ds): (TaskIdParam, Store<C>),
) -> Result<Json<GetTaskUploadMetricsResp>, Error> {
    Ok(Json(GetTaskUploadMetricsResp(
        ds.run_tx("get_task_upload_metrics", |tx| {
            Box::pin(async move { tx.get_task_upload_counter(&task_id).await })
        })
        .await?
        .ok_or(Error::NotFound)?,
    )))
}

pub(super) async fn get_global_hpke_configs<C: Clock>(
    ds: Store<C>,
) -> Result<Json<Vec<GlobalHpkeConfigResp>>, Error> {
    Ok(Json(
        ds.run_tx("get_global_hpke_configs", |tx| {
            Box::pin(async move { tx.get_global_hpke_keypairs().await })
        })
        .await?
        .into_iter()
        .map(GlobalHpkeConfigResp::from)
        .collect::<Vec<_>>(),
    ))
}

pub(super) async fn get_global_hpke_config<C: Clock>(
    (config_id, ds): (HpkeConfigIdParam, Store<C>),
) -> Result<Json<GlobalHpkeConfigResp>, Error> {
    Ok(Json(GlobalHpkeConfigResp::from(
        ds.run_tx("get_global_hpke_config", |tx| {
            Box::pin(async move { tx.get_global_hpke_keypair(&config_id).await })
        })
        .await?
        .ok_or(Error::NotFound)?,
    )))
}

pub(super) async fn put_global_hpke_config<C: Clock>(
    (ds, Json(req)): (Store<C>, Json<PutGlobalHpkeConfigReq>),
) -> Result<(Status, Json<GlobalHpkeConfigResp>), Error> {
    let existing_keypairs = ds
        .run_tx("put_global_hpke_config_determine_id", |tx| {
            Box::pin(async move { tx.get_global_hpke_keypairs().await })
        })
        .await?
        .iter()
        .map(|keypair| u8::from(*keypair.hpke_keypair().config().id()))
        .collect::<Vec<_>>();

    let config_id = HpkeConfigId::from(
        (0..=u8::MAX)
            .find(|i| !existing_keypairs.contains(i))
            .ok_or_else(|| {
                Error::Conflict("All possible IDs for global HPKE key have been taken".to_string())
            })?,
    );
    let keypair = generate_hpke_config_and_private_key(
        config_id,
        req.kem_id.unwrap_or(HpkeKemId::X25519HkdfSha256),
        req.kdf_id.unwrap_or(HpkeKdfId::HkdfSha256),
        req.aead_id.unwrap_or(HpkeAeadId::Aes128Gcm),
    )?;

    let inserted_keypair = ds
        .run_tx("put_global_hpke_config", |tx| {
            let keypair = keypair.clone();
            Box::pin(async move {
                tx.put_global_hpke_keypair(&keypair).await?;
                tx.get_global_hpke_keypair(&config_id).await
            })
        })
        .await?
        .ok_or_else(|| Error::Internal("Newly inserted key disappeared".to_string()))?;

    Ok((
        Status::Created,
        Json(GlobalHpkeConfigResp::from(inserted_keypair)),
    ))
}

pub(super) async fn patch_global_hpke_config<C: Clock>(
    (Json(req), config_id, ds): (Json<PatchGlobalHpkeConfigReq>, HpkeConfigIdParam, Store<C>),
) -> Result<Status, Error> {
    ds.run_tx("patch_hpke_global_keypair", |tx| {
        Box::pin(async move {
            tx.set_global_hpke_keypair_state(&config_id, &req.state)
                .await
        })
    })
    .await?;

    Ok(Status::Ok)
}

pub(super) async fn delete_global_hpke_config<C: Clock>(
    (config_id, ds): (HpkeConfigIdParam, Store<C>),
) -> Result<Status, Error> {
    match ds
        .run_tx("delete_global_hpke_config", |tx| {
            Box::pin(async move { tx.delete_global_hpke_keypair(&config_id).await })
        })
        .await
    {
        Ok(_) | Err(datastore::Error::MutationTargetNotFound) => Ok(Status::NoContent),
        Err(err) => Err(err.into()),
    }
}

pub(super) async fn get_taskprov_peer_aggregators<C: Clock>(
    ds: Store<C>,
) -> Result<Json<Vec<TaskprovPeerAggregatorResp>>, Error> {
    Ok(Json(
        ds.run_tx("get_taskprov_peer_aggregators", |tx| {
            Box::pin(async move { tx.get_taskprov_peer_aggregators().await })
        })
        .await?
        .into_iter()
        .map(TaskprovPeerAggregatorResp::from)
        .collect::<Vec<_>>(),
    ))
}

/// Inserts a new peer aggregator. Insertion is only supported, attempting to modify an existing
/// peer aggregator will fail.
///
/// TODO(1685): Requiring that we delete an existing peer aggregator before we can change it makes
/// token rotation cumbersome and fragile. Since token rotation is the main use case for updating
/// an existing peer aggregator, we will resolve peer aggregator updates in that issue.
pub(super) async fn post_taskprov_peer_aggregator<C: Clock>(
    (ds, Json(req)): (Store<C>, Json<PostTaskprovPeerAggregatorReq>),
) -> Result<(Status, Json<TaskprovPeerAggregatorResp>), Error> {
    let to_insert = PeerAggregator::new(
        req.endpoint,
        req.role,
        req.verify_key_init,
        req.collector_hpke_config,
        req.report_expiry_age,
        req.tolerable_clock_skew,
        req.aggregator_auth_tokens,
        req.collector_auth_tokens,
    );

    let inserted = ds
        .run_tx("post_taskprov_peer_aggregator", |tx| {
            let to_insert = to_insert.clone();
            Box::pin(async move {
                tx.put_taskprov_peer_aggregator(&to_insert).await?;
                tx.get_taskprov_peer_aggregator(to_insert.endpoint(), to_insert.role())
                    .await
            })
        })
        .await?
        .map(TaskprovPeerAggregatorResp::from)
        .ok_or_else(|| Error::Internal("Newly inserted peer aggregator disappeared".to_string()))?;

    Ok((Status::Created, Json(inserted)))
}

pub(super) async fn delete_taskprov_peer_aggregator<C: Clock>(
    (ds, req): (Store<C>, Json<DeleteTaskprovPeerAggregatorReq>),
) -> Result<Status, Error> {
    match ds
        .run_tx("delete_taskprov_peer_aggregator", |tx| {
            let req = req.clone();
            Box::pin(async move {
                tx.delete_taskprov_peer_aggregator(&req.endpoint, &req.role)
                    .await
            })
        })
        .await
    {
        Ok(_) | Err(datastore::Error::MutationTargetNotFound) => Ok(Status::NoContent),
        Err(err) => Err(err.into()),
    }
}
