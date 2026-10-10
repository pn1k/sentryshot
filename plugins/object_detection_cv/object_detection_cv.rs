// SPDX-License-Identifier: GPL-2.0-or-later

mod background;
mod blobs;
mod config;
mod detector;
mod source;
mod tracker;

use crate::{
    config::{ObjectDetectionCvConfig, set_enable},
    detector::{Detector, to_detections},
};
use async_trait::async_trait;
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, patch},
};
use common::{
    ArcLogger, ArcMsgLogger, Detection, Event, Label, LogEntry, LogLevel, LogSource, MonitorId,
    MsgLogger,
    monitor::{
        ArcMonitor, ArcMonitorManager, ArcSource, CreateEventDbError, DecoderError,
        SubscribeDecodedError,
    },
    recording::FrameRateLimiter,
    time::{DurationH264, UnixH264, UnixNano},
};
use plugin::{
    Application, Plugin, PreLoadPlugin,
    types::{Assets, Router},
};
use std::{
    borrow::Cow,
    collections::HashMap,
    ffi::c_char,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use thiserror::Error;
use tokio::runtime::Handle;
use tokio_util::{sync::CancellationToken, task::task_tracker::TaskTrackerToken};

#[unsafe(no_mangle)]
pub extern "C" fn version() -> *const c_char {
    plugin::get_version()
}

#[unsafe(no_mangle)]
pub extern "Rust" fn pre_load() -> Box<dyn PreLoadPlugin> {
    Box::new(PreLoadObjectDetectionCv)
}
struct PreLoadObjectDetectionCv;
impl PreLoadPlugin for PreLoadObjectDetectionCv {
    fn add_log_source(&self) -> Option<LogSource> {
        #[allow(clippy::unwrap_used)]
        Some("objectcv".try_into().unwrap())
    }
}

#[unsafe(no_mangle)]
pub extern "Rust" fn load(app: &dyn Application) -> Arc<dyn Plugin> {
    Arc::new(ObjectDetectionCvPlugin {
        rt_handle: app.rt_handle(),
        _task_token: app.task_token(),
        logger: app.logger(),
        monitor_manager: app.monitor_manager(),
        live: LiveDetectionsMap::default(),
    })
}

pub struct ObjectDetectionCvPlugin {
    rt_handle: Handle,
    _task_token: TaskTrackerToken,
    logger: ArcLogger,
    monitor_manager: ArcMonitorManager,
    live: LiveDetectionsMap,
}

// Current detections per monitor with an update timestamp and a freshness
// window. Stale entries are treated as empty by the handler. Detections are
// shared through an `Arc` so publishing a frame is a refcount bump and the
// handler clones nothing but the `Arc` under the lock.
type LiveDetectionsMap = Arc<Mutex<HashMap<MonitorId, (Instant, Duration, Arc<Vec<Detection>>)>>>;

const LIVE_TTL: Duration = Duration::from_secs(2);

const CV_MJS_FILE: &[u8] = include_bytes!("./js/objectDetectionCv.js");
const LIVE_DETECTIONS_MJS_FILE: &[u8] = include_bytes!("./js/liveDetections.js");

#[async_trait]
impl Plugin for ObjectDetectionCvPlugin {
    fn edit_assets(&self, assets: &mut Assets) {
        let js = assets["scripts/settings.js"].to_vec();
        *assets.get_mut("scripts/settings.js").expect("exist") = Cow::Owned(modify_settings_js(js));

        assets.insert(
            "scripts/objectDetectionCv.js".to_owned(),
            Cow::Borrowed(CV_MJS_FILE),
        );

        let live = assets["scripts/live.js"].to_vec();
        *assets.get_mut("scripts/live.js").expect("exist") = Cow::Owned(modify_live_js(live));

        assets.insert(
            "scripts/liveDetections.js".to_owned(),
            Cow::Borrowed(LIVE_DETECTIONS_MJS_FILE),
        );
    }

    async fn on_monitor_start(&self, token: CancellationToken, monitor: ArcMonitor) {
        let msg_logger = Arc::new(CvLogger {
            logger: self.logger.clone(),
            monitor_id: monitor.config().id().to_owned(),
        });

        if let Err(e) = self
            .start(
                token,
                msg_logger.clone(),
                self.live.clone(),
                monitor.clone(),
            )
            .await
        {
            msg_logger.log(LogLevel::Error, &format!("start: {e}"));
        }

        // The monitor stopped: drop its live-overlay entry so the map does
        // not grow without bound.
        self.live
            .lock()
            .expect("live detections mutex")
            .remove(&monitor.config().id().to_owned());
    }

    fn route(&self, router: Router) -> Router {
        let state = HandlerState {
            logger: self.logger.clone(),
            monitor_manager: self.monitor_manager.clone(),
            live: self.live.clone(),
        };
        router
            .route_user_no_csrf(
                "/api/monitor/{id}/object-detection-cv/detections",
                get(live_detections_handler).with_state(state.clone()),
            )
            .route_admin(
                "/api/monitor/{id}/object-detection-cv/enable",
                patch(enable_handler).with_state(state.clone()),
            )
            .route_admin(
                "/api/monitor/{id}/object-detection-cv/disable",
                patch(disable_handler).with_state(state),
            )
    }
}

struct CvLogger {
    logger: ArcLogger,
    monitor_id: MonitorId,
}

impl MsgLogger for CvLogger {
    fn log(&self, level: LogLevel, msg: &str) {
        self.logger
            .log(LogEntry::new(level, "objectcv", &self.monitor_id, msg));
    }
}

#[derive(Debug, Error)]
enum StartError {
    #[error("parse config: {0}")]
    ParseConfig(#[from] crate::config::ParseConfigError),

    #[error("failed to get sub-stream")]
    GetSubStream,
}

#[derive(Debug, Error)]
enum RunError {
    #[error("try_from: {0}")]
    TryFrom(#[from] std::num::TryFromIntError),

    #[error("subscribe: {0}")]
    Subscribe(#[from] SubscribeDecodedError),

    #[error("decoder: {0}")]
    Decoder(#[from] DecoderError),

    #[error("extract gray: {0}")]
    Gray(#[from] crate::source::GrayError),

    #[error("send event: {0}")]
    SendEvent(#[from] CreateEventDbError),

    #[error("detector panicked: {0}")]
    DetectorPanic(String),

    #[error("input size zero")]
    InputSizeZero,
}

impl ObjectDetectionCvPlugin {
    async fn start(
        &self,
        token: CancellationToken,
        msg_logger: ArcMsgLogger,
        live: LiveDetectionsMap,
        monitor: ArcMonitor,
    ) -> Result<(), StartError> {
        let config = monitor.config();

        let Some(config) = ObjectDetectionCvConfig::parse(config.raw().clone())? else {
            // Detection is disabled.
            return Ok(());
        };

        let source = if config.use_sub_stream {
            match monitor.source_sub().await {
                Some(Some(v)) => v,
                Some(None) => return Err(StartError::GetSubStream),
                None => {
                    // Cancelled.
                    return Ok(());
                }
            }
        } else {
            match monitor.source_main().await {
                Some(v) => v,
                None => {
                    // Cancelled.
                    return Ok(());
                }
            }
        };

        msg_logger.log(
            LogLevel::Info,
            &format!("using {}-stream", source.stream_type().name()),
        );

        loop {
            msg_logger.log(LogLevel::Debug, "run");
            if let Err(e) = self
                .run(&msg_logger, &live, &monitor, &config, &source)
                .await
            {
                msg_logger.log(LogLevel::Error, &format!("run: {e}"));
            }

            let sleep = || {
                let _enter = self.rt_handle.enter();
                tokio::time::sleep(Duration::from_secs(3))
            };
            tokio::select! {
                () = token.cancelled() => return Ok(()),
                () = sleep() => {}
            }
        }
    }

    async fn run(
        &self,
        msg_logger: &ArcMsgLogger,
        live: &LiveDetectionsMap,
        monitor: &ArcMonitor,
        config: &ObjectDetectionCvConfig,
        source: &ArcSource,
    ) -> Result<(), RunError> {
        use RunError::*;
        let Some(muxer) = source.muxer().await else {
            // Cancelled.
            return Ok(());
        };
        let params = muxer.params();
        if params.width == 0 || params.height == 0 {
            return Err(InputSizeZero);
        }

        let limiter = FrameRateLimiter::new(u64::try_from(*DurationH264::from(*config.feed_rate))?);

        let Some(feed) = source
            .subscribe_decoded(self.rt_handle.clone(), msg_logger.clone(), Some(limiter))
            .await
        else {
            // Cancelled.
            return Ok(());
        };
        let mut feed = feed?;

        let mut detector = Detector::new(config.clone(), msg_logger.clone());
        let label: Label = "object".to_owned().try_into().expect("valid");
        let monitor_id = monitor.config().id().to_owned();

        // Live-overlay freshness window: a few frame periods at least, so a
        // low feed rate does not make the overlay flicker between polls.
        let period = u64::try_from(DurationH264::from(*config.feed_rate).as_nanos()).unwrap_or(0);
        let ttl = LIVE_TTL.max(Duration::from_nanos(period).saturating_mul(3));

        loop {
            let Some(frame) = feed.recv().await else {
                // Feed was cancelled.
                return Ok(());
            };
            let frame = frame?;
            let time = UnixNano::from(UnixH264::new(frame.pts()));

            let (det, triggers) = match self
                .rt_handle
                .spawn_blocking(move || {
                    let result = detector.step(&frame);
                    (detector, result)
                })
                .await
            {
                Ok(v) => v,
                // A panic inside the detector must not take the plugin down:
                // surface it as an error so `run` restarts cleanly.
                Err(e) => return Err(RunError::DetectorPanic(e.to_string())),
            };
            detector = det;
            let triggers = triggers?;

            // Publish the current state for the live overlay.
            let current = detector.current();
            {
                let mut map = live.lock().expect("live detections mutex");
                if let Some(entry) = map.get_mut(&monitor_id) {
                    entry.0 = Instant::now();
                    entry.2 = current;
                } else {
                    map.insert(monitor_id.clone(), (Instant::now(), ttl, current));
                }
            }

            if triggers.is_empty() {
                continue;
            }

            let (aw, ah) = detector.dims();
            let detections = to_detections(&triggers, aw, ah, &label);

            msg_logger.log(
                LogLevel::Debug,
                &format!("trigger: {} object(s)", detections.len()),
            );

            monitor
                .trigger(
                    *config.trigger_duration,
                    Event {
                        time,
                        duration: *config.feed_rate,
                        detections,
                        source: Some("objcv".to_owned().try_into().expect("valid")),
                    },
                )
                .await?;
        }
    }
}

// Returns the current live detections of a monitor, or an empty list when the
// state is stale (detection stopped) or unset (detection disabled).
async fn live_detections_handler(
    State(s): State<HandlerState>,
    Path(monitor_id): Path<MonitorId>,
) -> Response {
    let Some(Some(_)) = s.monitor_manager.monitor_config(monitor_id.clone()).await else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let detections = {
        let map = s.live.lock().expect("live detections mutex");
        match map.get(&monitor_id) {
            Some((at, ttl, detections)) if at.elapsed() < *ttl => Arc::clone(detections),
            _ => Arc::new(Vec::new()),
        }
    };
    Json(detections.as_slice()).into_response()
}

fn modify_settings_js(tpl: Vec<u8>) -> Vec<u8> {
    const IMPORT_STATEMENT: &str = "import { objectDetectionCv } from \"./objectDetectionCv.js\";";
    const TARGET: &str = "/* SETTINGS_LAST_MONITOR_FIELD */";

    let tpl = String::from_utf8(tpl).expect("template should be valid utf8");
    let tpl = tpl.replace(
        TARGET,
        &("monitorFields.objectDetectionCv = objectDetectionCv(getMonitorId);\n".to_owned()
            + TARGET),
    );
    let tpl = IMPORT_STATEMENT.to_owned() + &tpl;
    tpl.as_bytes().to_owned()
}

fn modify_live_js(tpl: Vec<u8>) -> Vec<u8> {
    const IMPORT_STATEMENT: &str = "import { newLiveDetections } from \"./liveDetections.js\";";
    const TARGET: &str = "feeds.push(newStreamer(monitor, preferLowRes, streamer, buttons));";
    const REPLACEMENT: &str = "feeds.push(\n\t\t\t\t\tnewLiveDetections(\n\t\t\t\t\t\tnewStreamer(monitor, preferLowRes, streamer, buttons),\n\t\t\t\t\t\tmonitor,\n\t\t\t\t\t),\n\t\t\t\t);";

    let tpl = String::from_utf8(tpl).expect("template should be valid utf8");
    let tpl = tpl.replace(TARGET, REPLACEMENT);
    let tpl = IMPORT_STATEMENT.to_owned() + &tpl;
    tpl.as_bytes().to_owned()
}

#[derive(Clone)]
struct HandlerState {
    logger: ArcLogger,
    monitor_manager: ArcMonitorManager,
    live: LiveDetectionsMap,
}

async fn enable_handler(
    State(s): State<HandlerState>,
    Path(monitor_id): Path<MonitorId>,
) -> Response {
    let Some(Some(old_config)) = s.monitor_manager.monitor_config(monitor_id.clone()).await else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let Some(new_config) = set_enable(&old_config, true) else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to find enable field",
        )
            .into_response();
    };

    match s.monitor_manager.monitor_set_and_restart(new_config).await {
        Some(Ok(_)) => {
            s.logger.log(LogEntry::new(
                LogLevel::Info,
                "objectcv",
                &monitor_id,
                "detector enabled",
            ));
            StatusCode::OK.into_response()
        }
        Some(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        // Cancelled.
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn disable_handler(
    State(s): State<HandlerState>,
    Path(monitor_id): Path<MonitorId>,
) -> Response {
    let Some(Some(old_config)) = s.monitor_manager.monitor_config(monitor_id.clone()).await else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let new_config = match set_enable(&old_config, false) {
        Some(v) => v,
        // None means that it's already disabled in some way.
        None => old_config.clone(),
    };

    match s.monitor_manager.monitor_set_and_restart(new_config).await {
        Some(Ok(_)) => {
            s.logger.log(LogEntry::new(
                LogLevel::Info,
                "objectcv",
                &monitor_id,
                "detector disabled",
            ));

            StatusCode::OK.into_response()
        }
        Some(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        // Cancelled.
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
