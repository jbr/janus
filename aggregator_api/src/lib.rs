//! This crate implements the Janus aggregator API.
use janus_aggregator_core::{
    datastore::{Datastore as JanusDatastore, Error},
    task::Task,
    SecretBytes,
};
use janus_core::{hpke::generate_hpke_config_and_private_key, time::Clock};
use janus_messages::{Duration, HpkeAeadId, HpkeKdfId, HpkeKemId, TaskId, Time};
use models::{GetTaskIdsResp, PostTaskReq};
use models::{GetTaskMetricsResp, TaskResp};
use querystring::querify;
use rand::{distributions::Standard, random, thread_rng, Rng};
use ring::constant_time;
use std::{ops::Deref, str::FromStr, sync::Arc};
use trillium_opentelemetry::metrics;
use trillium::{Conn, Handler, KnownHeaderName, Status};
use trillium_api::{api, Halt, Json, State};
use trillium_router::{Router, RouterConnExt};
use trillium_tracing::Tracer;

/// Represents the configuration for an instance of the Aggregator API.
#[derive(Clone)]
pub struct Config {
    pub auth_tokens: Arc<Vec<SecretBytes>>,
}

struct TaskIdParam(TaskId);
#[trillium::async_trait]
impl trillium_api::FromConn for TaskIdParam {
    async fn from_conn(conn: &mut Conn) -> Option<Self> {
        TaskId::from_str(conn.param("task_id")?).ok().map(Self)
    }
}
impl Deref for TaskIdParam {
    type Target = TaskId;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Clone)]
struct Datastore<C: Clock>(Arc<JanusDatastore<C>>);
impl<C: Clock> Deref for Datastore<C> {
    type Target = JanusDatastore<C>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[trillium::async_trait]
impl<C: Clock> trillium_api::FromConn for Datastore<C> {
    async fn from_conn(conn: &mut Conn) -> Option<Self> {
        conn.take_state::<Arc<JanusDatastore<C>>>().map(Self)
    }
}

/// Returns a new handler for an instance of the aggregator API, backed by the given datastore,
/// according to the given configuration.
pub fn aggregator_api_handler<C: Clock>(ds: JanusDatastore<C>, cfg: Config) -> impl Handler {
    (
        State(cfg),
        State::new(Datastore(Arc::new(ds))),
        Tracer::new(),
        metrics("janus_aggregator_api").with_route(|conn| conn.route().map(ToString::to_string)),
        api(check_auth),
        Router::new()
            .get("/task_ids", api(get_task_ids::<C>))
            .post("/tasks", api(post_task::<C>))
            .get("/tasks/:task_id", api(get_task::<C>))
            .delete("/tasks/:task_id", api(delete_task::<C>))
            .get("/tasks/:task_id/metrics", api(get_task_metrics::<C>)),
    )
}

async fn check_auth(conn: &mut Conn, State(config): State<Config>) -> impl Handler {
    if let Some(authorization_value) = conn.headers().get(KnownHeaderName::Authorization) {
        if let Some(received_token) = authorization_value.as_ref().strip_prefix(b"Basic ") {
            if config.auth_tokens.iter().any(|key| {
                constant_time::verify_slices_are_equal(received_token, key.as_ref()).is_ok()
            }) {
                // Authorization succeeds.
                return None;
            }
        }
    }

    // Authorization fails.
    Some((Status::Unauthorized, Halt))
}

async fn get_task_ids<C: Clock>(conn: &mut Conn, ds: Datastore<C>) -> Result<impl Handler, Status> {
    const PAGINATION_TOKEN_KEY: &str = "pagination_token";

    let lower_bound = querify(conn.querystring())
        .into_iter()
        .find(|&(k, _)| k == PAGINATION_TOKEN_KEY)
        .map(|(_, v)| TaskId::from_str(v))
        .transpose()
        .map_err(|_| Status::BadRequest)?;

    let task_ids = ds
        .run_tx_with_name("get_task_ids", |tx| {
            Box::pin(async move { tx.get_task_ids(lower_bound).await })
        })
        .await
        .map_err(|_| Status::InternalServerError)?;

    let pagination_token = task_ids.last().cloned();

    Ok(Json(GetTaskIdsResp {
        task_ids,
        pagination_token,
    }))
}

async fn post_task<C: Clock>(
    _: &mut Conn,
    (ds, Json(req)): (Datastore<C>, Json<PostTaskReq>),
) -> Result<impl Handler, Status> {
    let vdaf_verify_keys = Vec::from([SecretBytes::new(
        thread_rng()
            .sample_iter(Standard)
            .take(req.vdaf.verify_key_length())
            .collect(),
    )]);
    let task_expiration = Time::from_seconds_since_epoch(req.task_expiration);
    let time_precision = Duration::from_seconds(req.time_precision);
    let hpke_keys = Vec::from([generate_hpke_config_and_private_key(
        random(),
        HpkeKemId::X25519HkdfSha256,
        HpkeKdfId::HkdfSha256,
        HpkeAeadId::Aes128Gcm,
    )]);

    let task = Task::new(
        /* task_id */ random(),
        /* aggregator_endpoints */ req.aggregator_endpoints,
        /* query_type */ req.query_type,
        /* vdaf */ req.vdaf,
        /* role */ req.role,
        /* vdaf_verify_keys */ vdaf_verify_keys,
        /* max_batch_query_count */ req.max_batch_query_count,
        /* task_expiration */ task_expiration,
        /* report_expiry_age */
        Some(Duration::from_seconds(3600 * 24 * 7 * 2)), // 2 weeks
        /* min_batch_size */ req.min_batch_size,
        /* time_precision */ time_precision,
        /* tolerable_clock_skew */
        Duration::from_seconds(60), // 1 minute,
        /* collector_hpke_config */ req.collector_hpke_config,
        /* aggregator_auth_tokens */ Vec::from([random()]),
        /* collector_auth_tokens */ Vec::from([random()]),
        /* hpke_keys */ hpke_keys,
    )
    .map(Arc::new)
    .map_err(|_| Status::BadRequest)?;

    ds.run_tx_with_name("post_task", |tx| {
        let task = Arc::clone(&task);
        Box::pin(async move { tx.put_task(&task).await })
    })
    .await
    .map_err(|_| Status::InternalServerError)?;

    Ok(Json(TaskResp::from(&*task)))
}

async fn get_task<C: Clock>(
    _: &mut Conn,
    (ds, TaskIdParam(task_id)): (Datastore<C>, TaskIdParam),
) -> Result<impl Handler, Status> {
    ds.run_tx_with_name("get_task", |tx| {
        Box::pin(async move { tx.get_task(&task_id).await })
    })
    .await
    .map_err(|_| Status::InternalServerError)?
    .ok_or(Status::NotFound)
    .map(|task| Json(TaskResp::from(&task)))
}

async fn delete_task<C: Clock>(
    _: &mut Conn,
    (ds, TaskIdParam(task_id)): (Datastore<C>, TaskIdParam),
) -> Status {
    ds.run_tx_with_name("delete_task", |tx| {
        Box::pin(async move { tx.delete_task(&task_id).await })
    })
    .await
    .map(|_| Status::Ok)
    .unwrap_or_else(|e| match e {
        Error::MutationTargetNotFound => Status::NotFound,
        _ => Status::InternalServerError,
    })
}

async fn get_task_metrics<C: Clock>(
    _: &mut Conn,
    (ds, TaskIdParam(task_id)): (Datastore<C>, TaskIdParam),
) -> Result<impl Handler, Status> {
    ds.run_tx_with_name("get_task_metrics", |tx| {
        Box::pin(async move { tx.get_task_metrics(task_id).await })
    })
    .await
    .map_err(|_| Status::InternalServerError)?
    .ok_or(Status::NotFound)
    .map(|(reports, report_aggregations)| {
        Json(GetTaskMetricsResp {
            reports,
            report_aggregations,
        })
    })
}

mod models {
    use std::collections::HashMap;

    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use janus_aggregator_core::task::{QueryType, Task};
    use janus_core::task::VdafInstance;
    use janus_messages::{Duration, HpkeConfig, HpkeConfigId, Role, TaskId, Time};
    use serde::{Deserialize, Serialize};
    use url::Url;

    #[derive(Serialize)]
    pub(crate) struct GetTaskIdsResp {
        pub(crate) task_ids: Vec<TaskId>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub(crate) pagination_token: Option<TaskId>,
    }

    #[derive(Serialize, Deserialize)]
    pub(crate) struct PostTaskReq {
        pub(crate) aggregator_endpoints: Vec<Url>,
        pub(crate) query_type: QueryType,
        pub(crate) vdaf: VdafInstance,
        pub(crate) role: Role,
        pub(crate) max_batch_query_count: u64,
        pub(crate) task_expiration: u64, // seconds since UNIX epoch
        pub(crate) min_batch_size: u64,
        pub(crate) time_precision: u64, // seconds
        pub(crate) collector_hpke_config: HpkeConfig,
    }

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub(crate) struct TaskResp {
        pub(crate) task_id: TaskId,
        pub(crate) aggregator_endpoints: Vec<Url>,
        pub(crate) query_type: QueryType,
        pub(crate) vdaf: VdafInstance,
        pub(crate) role: Role,
        pub(crate) vdaf_verify_keys: Vec<String>,
        pub(crate) max_batch_query_count: u64,
        pub(crate) task_expiration: Time,
        pub(crate) report_expiry_age: Option<Duration>,
        pub(crate) min_batch_size: u64,
        pub(crate) time_precision: Duration,
        pub(crate) tolerable_clock_skew: Duration,
        pub(crate) collector_hpke_config: HpkeConfig,
        pub(crate) aggregator_auth_tokens: Vec<String>,
        pub(crate) collector_auth_tokens: Vec<String>,
        pub(crate) aggregator_hpke_configs: HashMap<HpkeConfigId, HpkeConfig>,
    }

    impl From<&Task> for TaskResp {
        fn from(task: &Task) -> Self {
            let encoded_verify_keys: Vec<_> = task
                .vdaf_verify_keys()
                .iter()
                .map(|key| URL_SAFE_NO_PAD.encode(key))
                .collect();
            let encoded_aggregator_auth_tokens: Vec<_> = task
                .aggregator_auth_tokens()
                .iter()
                .map(|token| URL_SAFE_NO_PAD.encode(token))
                .collect();
            let encoded_collector_auth_tokens: Vec<_> = task
                .collector_auth_tokens()
                .iter()
                .map(|token| URL_SAFE_NO_PAD.encode(token))
                .collect();
            let aggregator_hpke_configs: HashMap<_, _> = task
                .hpke_keys()
                .iter()
                .map(|(&config_id, keypair)| (config_id, keypair.config().clone()))
                .collect();

            Self {
                task_id: *task.id(),
                aggregator_endpoints: task.aggregator_endpoints().to_vec(),
                query_type: *task.query_type(),
                vdaf: task.vdaf().clone(),
                role: *task.role(),
                vdaf_verify_keys: encoded_verify_keys,
                max_batch_query_count: task.max_batch_query_count(),
                task_expiration: *task.task_expiration(),
                report_expiry_age: task.report_expiry_age().cloned(),
                min_batch_size: task.min_batch_size(),
                time_precision: *task.time_precision(),
                tolerable_clock_skew: *task.tolerable_clock_skew(),
                collector_hpke_config: task.collector_hpke_config().clone(),
                aggregator_auth_tokens: encoded_aggregator_auth_tokens,
                collector_auth_tokens: encoded_collector_auth_tokens,
                aggregator_hpke_configs,
            }
        }
    }

    #[derive(Serialize)]
    pub(crate) struct GetTaskMetricsResp {
        pub(crate) reports: u64,
        pub(crate) report_aggregations: u64,
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        aggregator_api_handler,
        models::{GetTaskIdsResp, GetTaskMetricsResp, PostTaskReq, TaskResp},
        Config,
    };
    use futures::future::try_join_all;
    use janus_aggregator_core::{
        datastore::{
            models::{
                AggregationJob, AggregationJobState, LeaderStoredReport, ReportAggregation,
                ReportAggregationState,
            },
            test_util::{ephemeral_datastore, EphemeralDatastore},
        },
        task::{test_util::TaskBuilder, QueryType},
        SecretBytes,
    };
    use janus_core::{
        hpke::generate_hpke_config_and_private_key,
        task::VdafInstance,
        test_util::{
            dummy_vdaf::{self, AggregationParam},
            install_test_trace_subscriber,
        },
        time::MockClock,
    };
    use janus_messages::{
        query_type::TimeInterval, AggregationJobRound, Duration, HpkeAeadId, HpkeKdfId, HpkeKemId,
        Interval, Role, TaskId, Time,
    };
    use rand::random;
    use std::iter;
    use trillium::{Handler, Status};
    use trillium_testing::{
        assert_response, assert_status,
        prelude::{delete, get, post},
    };

    const AUTH_TOKEN: &str = "auth_token";

    async fn setup_api_test() -> (impl Handler, EphemeralDatastore) {
        install_test_trace_subscriber();
        let ephemeral_datastore = ephemeral_datastore().await;
        let handler = aggregator_api_handler(
            ephemeral_datastore.datastore(MockClock::default()),
            Config {
                auth_tokens: Vec::from([SecretBytes::new(AUTH_TOKEN.as_bytes().to_vec())]),
            },
        );

        (handler, ephemeral_datastore)
    }

    #[tokio::test]
    async fn get_task_ids() {
        // Setup: write a few tasks to the datastore.
        let (handler, ephemeral_datastore) = setup_api_test().await;
        let ds = ephemeral_datastore.datastore(MockClock::default());

        let mut task_ids: Vec<_> = ds
            .run_tx(|tx| {
                Box::pin(async move {
                    let tasks: Vec<_> = iter::repeat_with(|| {
                        TaskBuilder::new(QueryType::TimeInterval, VdafInstance::Fake, Role::Leader)
                            .build()
                    })
                    .take(10)
                    .collect();

                    try_join_all(tasks.iter().map(|task| tx.put_task(task))).await?;

                    Ok(tasks.into_iter().map(|task| *task.id()).collect())
                })
            })
            .await
            .unwrap();
        task_ids.sort();

        fn response_for(task_ids: &[TaskId]) -> String {
            serde_json::to_string(&GetTaskIdsResp {
                task_ids: task_ids.to_vec(),
                pagination_token: task_ids.last().cloned(),
            })
            .unwrap()
        }

        // Verify: we can get the task IDs we wrote back from the API.
        assert_response!(
            get("/task_ids")
                .with_request_header("Authorization", format!("Basic {}", AUTH_TOKEN))
                .run_async(&handler)
                .await,
            Status::Ok,
            response_for(&task_ids),
        );

        // Verify: the lower_bound is respected, if specified.
        assert_response!(
            get(&format!(
                "/task_ids?pagination_token={}",
                task_ids.first().unwrap()
            ))
            .with_request_header("Authorization", format!("Basic {}", AUTH_TOKEN))
            .run_async(&handler)
            .await,
            Status::Ok,
            response_for(&task_ids[1..]),
        );

        // Verify: if the lower bound is large enough, nothing is returned.
        // (also verifies the "last" response will not include a pagination token)
        assert_response!(
            get(&format!(
                "/task_ids?pagination_token={}",
                task_ids.last().unwrap()
            ))
            .with_request_header("Authorization", format!("Basic {}", AUTH_TOKEN))
            .run_async(&handler)
            .await,
            Status::Ok,
            response_for(&[]),
        );

        // Verify: unauthorized requests are denied appropriately.
        assert_response!(
            get("/task_ids").run_async(&handler).await,
            Status::Unauthorized,
            "",
        );
    }

    #[tokio::test]
    async fn post_task() {
        // Setup: create a datastore & handler.
        let (handler, ephemeral_datastore) = setup_api_test().await;
        let ds = ephemeral_datastore.datastore(MockClock::default());

        // Verify: posting a task creates a new task which matches the request.
        let req = PostTaskReq {
            aggregator_endpoints: Vec::from([
                "http://leader.endpoint".try_into().unwrap(),
                "http://helper.endpoint".try_into().unwrap(),
            ]),
            query_type: QueryType::TimeInterval,
            vdaf: VdafInstance::Prio3Count,
            role: Role::Leader,
            max_batch_query_count: 12,
            task_expiration: 12345,
            min_batch_size: 223,
            time_precision: 62,
            collector_hpke_config: generate_hpke_config_and_private_key(
                random(),
                HpkeKemId::X25519HkdfSha256,
                HpkeKdfId::HkdfSha256,
                HpkeAeadId::Aes128Gcm,
            )
            .config()
            .clone(),
        };
        let mut conn = post("/tasks")
            .with_request_body(serde_json::to_vec(&req).unwrap())
            .with_request_header("Authorization", format!("Basic {}", AUTH_TOKEN))
            .run_async(&handler)
            .await;
        assert_status!(conn, Status::Ok);
        let got_task_resp: TaskResp = serde_json::from_slice(
            &conn
                .take_response_body()
                .unwrap()
                .into_bytes()
                .await
                .unwrap(),
        )
        .unwrap();

        let got_task = ds
            .run_tx(|tx| {
                let got_task_resp = got_task_resp.clone();
                Box::pin(async move { tx.get_task(&got_task_resp.task_id).await })
            })
            .await
            .unwrap()
            .expect("task was not created");

        // Verify that the task written to the datastore matches the request...
        assert_eq!(&req.aggregator_endpoints, got_task.aggregator_endpoints());
        assert_eq!(&req.query_type, got_task.query_type());
        assert_eq!(&req.vdaf, got_task.vdaf());
        assert_eq!(&req.role, got_task.role());
        assert_eq!(req.max_batch_query_count, got_task.max_batch_query_count());
        assert_eq!(
            &Time::from_seconds_since_epoch(req.task_expiration),
            got_task.task_expiration()
        );
        assert_eq!(req.min_batch_size, got_task.min_batch_size());
        assert_eq!(
            &Duration::from_seconds(req.time_precision),
            got_task.time_precision()
        );
        assert_eq!(&req.collector_hpke_config, got_task.collector_hpke_config());

        // ...and the response.
        assert_eq!(got_task_resp, TaskResp::from(&got_task));

        // Verify: unauthorized requests are denied appropriately.
        assert_response!(
            post("/tasks")
                .with_request_body(serde_json::to_vec(&req).unwrap())
                .run_async(&handler)
                .await,
            Status::Unauthorized,
            "",
        );
    }

    #[tokio::test]
    async fn get_task() {
        // Setup: write a task to the datastore.
        let (handler, ephemeral_datastore) = setup_api_test().await;
        let ds = ephemeral_datastore.datastore(MockClock::default());

        let task =
            TaskBuilder::new(QueryType::TimeInterval, VdafInstance::Fake, Role::Leader).build();

        ds.run_tx(|tx| {
            let task = task.clone();
            Box::pin(async move {
                tx.put_task(&task).await?;
                Ok(())
            })
        })
        .await
        .unwrap();

        // Verify: getting the task returns the expected result.
        let want_task_resp = TaskResp::from(&task);
        let mut conn = get(&format!("/tasks/{}", task.id()))
            .with_request_header("Authorization", format!("Basic {}", AUTH_TOKEN))
            .run_async(&handler)
            .await;
        assert_status!(conn, Status::Ok);
        let got_task_resp = serde_json::from_slice(
            &conn
                .take_response_body()
                .unwrap()
                .into_bytes()
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(want_task_resp, got_task_resp);

        // Verify: getting a nonexistent task returns NotFound.
        assert_response!(
            get(&format!("/tasks/{}", random::<TaskId>()))
                .with_request_header("Authorization", format!("Basic {}", AUTH_TOKEN))
                .run_async(&handler)
                .await,
            Status::NotFound,
            "",
        );

        // Verify: unauthorized requests are denied appropriately.
        assert_response!(
            get(&format!("/tasks/{}", task.id()))
                .run_async(&handler)
                .await,
            Status::Unauthorized,
            "",
        );
    }

    #[tokio::test]
    async fn delete_task() {
        // Setup: write a task to the datastore.
        let (handler, ephemeral_datastore) = setup_api_test().await;
        let ds = ephemeral_datastore.datastore(MockClock::default());

        let task_id = ds
            .run_tx(|tx| {
                Box::pin(async move {
                    let task =
                        TaskBuilder::new(QueryType::TimeInterval, VdafInstance::Fake, Role::Leader)
                            .build();

                    tx.put_task(&task).await?;

                    Ok(*task.id())
                })
            })
            .await
            .unwrap();

        // Verify: deleting a task succeeds (and actually deletes the task).
        assert_response!(
            delete(&format!("/tasks/{}", &task_id))
                .with_request_header("Authorization", format!("Basic {}", AUTH_TOKEN))
                .run_async(&handler)
                .await,
            Status::Ok,
            "",
        );

        ds.run_tx(|tx| {
            Box::pin(async move {
                assert_eq!(tx.get_task(&task_id).await.unwrap(), None);
                Ok(())
            })
        })
        .await
        .unwrap();

        // Verify: deleting a task twice returns NotFound.
        assert_response!(
            delete(&format!("/tasks/{}", &task_id))
                .with_request_header("Authorization", format!("Basic {}", AUTH_TOKEN))
                .run_async(&handler)
                .await,
            Status::NotFound,
            "",
        );

        // Verify: deleting an arbitrary nonexistent task ID returns NotFound.
        assert_response!(
            delete(&format!("/tasks/{}", &random::<TaskId>()))
                .with_request_header("Authorization", format!("Basic {}", AUTH_TOKEN))
                .run_async(&handler)
                .await,
            Status::NotFound,
            "",
        );

        // Verify: unauthorized requests are denied appropriately.
        assert_response!(
            delete(&format!("/tasks/{}", &task_id))
                .run_async(&handler)
                .await,
            Status::Unauthorized,
            ""
        );
    }

    #[tokio::test]
    async fn get_task_metrics() {
        // Setup: write a task, some reports, and some report aggregations to the datastore.
        const REPORT_COUNT: usize = 10;
        const REPORT_AGGREGATION_COUNT: usize = 4;

        let (handler, ephemeral_datastore) = setup_api_test().await;
        let ds = ephemeral_datastore.datastore(MockClock::default());
        let task_id = ds
            .run_tx(|tx| {
                Box::pin(async move {
                    let task =
                        TaskBuilder::new(QueryType::TimeInterval, VdafInstance::Fake, Role::Leader)
                            .build();
                    let task_id = *task.id();
                    tx.put_task(&task).await?;

                    let reports: Vec<_> = iter::repeat_with(|| {
                        LeaderStoredReport::new_dummy(task_id, Time::from_seconds_since_epoch(0))
                    })
                    .take(REPORT_COUNT)
                    .collect();
                    try_join_all(reports.iter().map(|report| async move {
                        tx.put_client_report(&dummy_vdaf::Vdaf::new(), report).await
                    }))
                    .await?;

                    let aggregation_job_id = random();
                    tx.put_aggregation_job(
                        &AggregationJob::<0, TimeInterval, dummy_vdaf::Vdaf>::new(
                            task_id,
                            aggregation_job_id,
                            AggregationParam(0),
                            (),
                            Interval::new(
                                Time::from_seconds_since_epoch(0),
                                Duration::from_seconds(1),
                            )
                            .unwrap(),
                            AggregationJobState::InProgress,
                            AggregationJobRound::from(0),
                        ),
                    )
                    .await?;

                    try_join_all(
                        reports
                            .iter()
                            .take(REPORT_AGGREGATION_COUNT)
                            .enumerate()
                            .map(|(ord, report)| async move {
                                tx.put_report_aggregation(
                                    &ReportAggregation::<0, dummy_vdaf::Vdaf>::new(
                                        task_id,
                                        aggregation_job_id,
                                        *report.metadata().id(),
                                        *report.metadata().time(),
                                        ord.try_into().unwrap(),
                                        ReportAggregationState::Start,
                                    ),
                                )
                                .await
                            }),
                    )
                    .await?;

                    Ok(task_id)
                })
            })
            .await
            .unwrap();

        // Verify: requesting metrics on a task returns the correct result.
        assert_response!(
            get(&format!("/tasks/{}/metrics", &task_id))
                .with_request_header("Authorization", format!("Basic {}", AUTH_TOKEN))
                .run_async(&handler)
                .await,
            Status::Ok,
            serde_json::to_string(&GetTaskMetricsResp {
                reports: REPORT_COUNT.try_into().unwrap(),
                report_aggregations: REPORT_AGGREGATION_COUNT.try_into().unwrap(),
            })
            .unwrap(),
        );

        // Verify: requesting metrics on a nonexistent task returns NotFound.
        assert_response!(
            delete(&format!("/tasks/{}", &random::<TaskId>()))
                .with_request_header("Authorization", format!("Basic {}", AUTH_TOKEN))
                .run_async(&handler)
                .await,
            Status::NotFound,
            "",
        );

        // Verify: unauthorized requests are denied appropriately.
        assert_response!(
            get(&format!("/tasks/{}/metrics", &task_id))
                .run_async(&handler)
                .await,
            Status::Unauthorized,
            "",
        );
    }
}
