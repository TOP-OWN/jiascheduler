use std::{
    collections::HashMap,
    sync::{Arc, atomic},
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Local};
use futures::{SinkExt, StreamExt};
use nanoid::nanoid;

use crate::{
    bridge::msg::{
        BundleOutputParams, RuntimeActionParams, SftpDownloadParams, SftpReadDirParams,
        SftpRemoveParams, SftpUploadParams, UpdateJobParams,
    },
    comet::types::SshLoginParams,
    get_comet_addr, get_local_ip, get_mac_address, run_id,
    scheduler::types::JobAction,
    set_comet_addr,
    ssh::{self, ConnectParams, Session},
};
use futures_util::stream::{SplitSink, SplitStream};

use serde_json::{Value, json};
use tokio::{
    net::TcpStream,
    select,
    sync::{
        Mutex,
        mpsc::{Receiver, Sender, UnboundedSender, channel, unbounded_channel},
    },
    task,
    time::{sleep, timeout},
};
use tokio_cron_scheduler::{Job, JobScheduler};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{ClientRequestBuilder, Message},
};
use tracing::{debug, error, info};
use uuid::Uuid;

use super::{
    executor::Ctx,
    file::try_download_file,
    types::{
        self, AssignUserOption, BundleOutput, RuntimeAction, ScheduleType, SshConnectionOption,
    },
};

use crate::{
    bridge::{
        Bridge,
        client::WsClient,
        msg::{DispatchJobParams, HeartbeatParams, MsgReqKind},
    },
    get_endpoint,
    scheduler::executor::Executor,
};

pub struct RunningJobContext {
    timer_running_counter: atomic::AtomicU32,
    once_running_counter: atomic::AtomicU32,
    once_kill_senders: Vec<(String, Sender<()>)>,
    timer_kill_senders: Vec<(String, Sender<()>)>,
    daemon_kill_senders: Vec<(String, Sender<()>)>,
}

#[derive(Clone)]
pub struct React {
    sched: JobScheduler,
    bridge: Bridge,
    output_dir: String,
    namespace: String,
    local_ip: String,
    client_key: String,
    schedule_uuid_mapping: Arc<Mutex<HashMap<String, Uuid>>>,
    supervisor_jobs: Arc<Mutex<HashMap<String, UnboundedSender<SupervisorSignal>>>>,
    running_job_contexts: Arc<Mutex<HashMap<String, RunningJobContext>>>,
}

pub enum SupervisorSignal {
    UpdateOptions(DispatchJobParams),
    Exit,
}

impl React {
    async fn new(
        bridge: Bridge,
        namespace: String,
        local_ip: String,
        client_key: String,
        output_dir: String,
    ) -> Self {
        Self {
            sched: JobScheduler::new().await.unwrap(),
            output_dir,
            schedule_uuid_mapping: Arc::new(Mutex::new(HashMap::new())),
            running_job_contexts: Arc::new(Mutex::new(HashMap::new())),
            supervisor_jobs: Arc::new(Mutex::new(HashMap::new())),
            bridge,
            client_key,
            namespace,
            local_ip,
        }
    }

    async fn send_update_job_msg(&self, data: UpdateJobParams) -> Result<Value> {
        self.send_bridge_msg(MsgReqKind::UpdateJobRequest(data))
            .await
    }

    async fn send_bridge_msg(&self, data: MsgReqKind) -> Result<Value> {
        self.bridge.send_msg(&self.client_key, data).await
    }

    async fn add_job_schedule(&mut self, eid: String, job: Job) -> Result<Option<DateTime<Local>>> {
        self.remove_job_schedule(eid.as_str()).await?;

        let mut locked_map = self.schedule_uuid_mapping.lock().await;
        if locked_map.get(&eid).is_some() {
            anyhow::bail!("{eid} is existed");
        }

        let uuid = self.sched.add(job).await?;

        let next_time = self.sched.next_tick_for_job(uuid).await?.map(|v| v.into());

        locked_map.insert(eid, uuid);
        Ok(next_time)
    }

    async fn remove_job_schedule(&mut self, eid: &str) -> Result<()> {
        let mut locked_map = self.schedule_uuid_mapping.lock().await;
        if let Some(uuid) = locked_map.get(eid) {
            self.sched.remove(uuid).await?;
            locked_map.remove(eid);
        }
        Ok(())
    }

    async fn can_execute(&mut self, params: &DispatchJobParams) -> Result<()> {
        let eid = params.base_job.eid.clone();
        let mut locked_map = self.running_job_contexts.lock().await;
        let max_parallel = params
            .base_job
            .max_parallel
            .map_or(1, |v| if v > 0 { v } else { 1 });

        if let Some(ctx) = locked_map.get_mut(&eid) {
            let parallel = match params.action {
                JobAction::StartTimer => ctx.timer_running_counter.load(atomic::Ordering::Relaxed),
                JobAction::Exec => ctx.once_running_counter.load(atomic::Ordering::Relaxed),
                _ => anyhow::bail!("invalid action"),
            };
            if parallel >= max_parallel {
                anyhow::bail!("job {} is running, max parallel {}", eid, max_parallel);
            }
            match params.action {
                JobAction::StartTimer => ctx
                    .timer_running_counter
                    .fetch_add(1, atomic::Ordering::Relaxed),
                JobAction::Exec => ctx
                    .once_running_counter
                    .fetch_add(1, atomic::Ordering::Relaxed),
                _ => anyhow::bail!("invalid action"),
            };
        } else {
            let ctx = RunningJobContext {
                timer_running_counter: atomic::AtomicU32::new(0),
                once_running_counter: atomic::AtomicU32::new(0),
                once_kill_senders: vec![],
                timer_kill_senders: vec![],
                daemon_kill_senders: vec![],
            };

            match params.action {
                JobAction::StartTimer => ctx
                    .timer_running_counter
                    .fetch_add(1, atomic::Ordering::Relaxed),
                JobAction::Exec => ctx
                    .once_running_counter
                    .fetch_add(1, atomic::Ordering::Relaxed),
                _ => anyhow::bail!("invalid action"),
            };

            locked_map.insert(eid, ctx);
        }

        Ok(())
    }

    async fn set_execute_context(
        &mut self,
        params: &DispatchJobParams,
        kill_signal_tx: Sender<()>,
    ) -> String {
        let eid = params.base_job.eid.clone();
        let run_id = nanoid!();
        let mut locked_map = self.running_job_contexts.lock().await;
        if let Some(ctx) = locked_map.get_mut(&eid) {
            match params.action {
                JobAction::StartTimer => ctx
                    .timer_kill_senders
                    .append(&mut vec![(run_id.clone(), kill_signal_tx)]),
                JobAction::Exec => ctx
                    .once_kill_senders
                    .append(&mut vec![(run_id.clone(), kill_signal_tx)]),
                JobAction::StartSupervising => ctx
                    .daemon_kill_senders
                    .push((run_id.clone(), kill_signal_tx)),
                _ => unreachable!(),
            };
        } else {
            let mut ctx = RunningJobContext {
                timer_running_counter: atomic::AtomicU32::new(0),
                once_running_counter: atomic::AtomicU32::new(0),
                once_kill_senders: vec![],
                timer_kill_senders: vec![],
                daemon_kill_senders: vec![],
            };

            match params.action {
                JobAction::StartTimer => ctx
                    .timer_kill_senders
                    .append(&mut vec![(run_id.clone(), kill_signal_tx)]),
                JobAction::Exec => ctx
                    .once_kill_senders
                    .append(&mut vec![(run_id.clone(), kill_signal_tx)]),
                JobAction::StartSupervising => ctx
                    .daemon_kill_senders
                    .push((run_id.clone(), kill_signal_tx)),
                _ => unreachable!(),
            };

            locked_map.insert(eid, ctx);
        }
        run_id
    }

    async fn end_execute(&mut self, params: &DispatchJobParams) {
        let eid = params.base_job.eid.clone();
        let run_id = params.run_id.clone();
        let mut locked_map = self.running_job_contexts.lock().await;
        if let Some(ctx) = locked_map.get_mut(&eid) {
            match params.action {
                JobAction::StartTimer => {
                    ctx.timer_running_counter
                        .fetch_sub(1, atomic::Ordering::Relaxed);
                    ctx.timer_kill_senders.retain(|(id, _)| *id != run_id);
                }
                JobAction::Exec => {
                    ctx.once_running_counter
                        .fetch_sub(1, atomic::Ordering::Relaxed);
                    ctx.once_kill_senders.retain(|(id, _)| *id != run_id);
                }
                JobAction::StartSupervising => {
                    ctx.daemon_kill_senders.retain(|(id, _)| *id != run_id)
                }
                _ => unreachable!(),
            }
        }
    }

    async fn kill_job(&mut self, eid: &str, schedule_type: ScheduleType) {
        let mut locked_map = self.running_job_contexts.lock().await;

        let Some(handler) = locked_map.get_mut(eid) else {
            return;
        };

        match schedule_type {
            ScheduleType::Timer => {
                for (_, tx) in &handler.timer_kill_senders {
                    if let Err(e) = tx.send(()).await {
                        error!("failed send kill signal, eid: {eid} {}", e);
                    }
                }
            }
            ScheduleType::Once => {
                for (_, tx) in &handler.once_kill_senders {
                    if let Err(e) = tx.send(()).await {
                        error!("failed send kill signal, eid: {eid} {}", e);
                    }
                }
            }
            ScheduleType::Daemon => {
                for (_, tx) in &handler.daemon_kill_senders {
                    if let Err(e) = tx.send(()).await {
                        error!("failed send kill signal, eid: {eid} {}", e);
                    }
                }
            }
            _ => unreachable!(),
        }
    }

    async fn start(&mut self) -> Result<()> {
        self.sched.start().await?;
        Ok(())
    }

    async fn update_supervising(
        &mut self,
        eid: String,
        opts: DispatchJobParams,
        tx: UnboundedSender<SupervisorSignal>,
    ) -> bool {
        let mut jobs = self.supervisor_jobs.lock().await;
        if let Some(v) = jobs.get(&eid) {
            let _ = v
                .send(SupervisorSignal::UpdateOptions(opts))
                .map_err(|v| error!("failed update supervising job options, {v:?}"));
            false
        } else {
            jobs.insert(eid, tx);
            true
        }
    }

    #[allow(unused)]
    async fn is_supervising(&self, eid: &String) -> bool {
        self.supervisor_jobs.lock().await.contains_key(eid)
    }

    async fn stop_supervising(&mut self, eid: &String) -> Result<()> {
        let mut jobs = self.supervisor_jobs.lock().await;
        let val = jobs.remove(eid);
        if let Some(supervisor_job) = val {
            let _ = supervisor_job
                .send(SupervisorSignal::Exit)
                .map_err(|v| error!("failed stop supervising job, {v:?}"));
        }
        Ok(())
    }
}

pub struct Scheduler<T> {
    comet_addr: Vec<String>,
    comet_secret: String,
    mac_addr: String,
    output_dir: String,
    is_initialized: bool,
    client: Option<T>,
    pub namespace: String,
    bridge: Bridge,
    ssh_connection_option: Option<SshConnectionOption>,
    assign_user_option: Option<AssignUserOption>,
}

impl
    Scheduler<
        WsClient<
            SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
            SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
        >,
    >
{
    pub fn new(
        namespace: String,
        comet_addr: Vec<String>,
        comet_secret: String,
        output_dir: String,
        ssh_connection_option: Option<SshConnectionOption>,
        assign_user_option: Option<AssignUserOption>,
    ) -> Self {
        Scheduler {
            comet_addr,
            comet_secret,
            output_dir,
            client: None,
            mac_addr: get_mac_address().expect("failed get mac address"),
            is_initialized: false,
            namespace,
            bridge: Bridge::new(),
            ssh_connection_option,
            assign_user_option,
        }
    }

    pub fn client_key(&self) -> String {
        get_endpoint(get_local_ip().to_string(), self.mac_addr.clone())
    }

    pub fn get_comet_addr(&mut self) -> String {
        if let Some(v) = self.comet_addr.pop() {
            self.comet_addr.push(v.clone());
            return v;
        }
        panic!("comet_addr is empty");
    }

    pub async fn ssh_poll(&mut self) {
        let comet_secret = self.comet_secret.clone();
        let mac_addr = self.mac_addr.clone();

        tokio::spawn(async move {
            loop {
                let addr = if let Some(addr) = get_comet_addr() {
                    addr
                } else {
                    sleep(Duration::from_millis(100)).await;
                    continue;
                };

                if let Err(e) =
                    Self::ssh_keepalive(addr.clone(), mac_addr.clone(), comet_secret.clone()).await
                {
                    error!("failed ssh keepalive {e}");
                    sleep(Duration::from_secs(1)).await;
                }
            }
        });
    }

    pub async fn ssh_keepalive(
        addr: String,
        mac_addr: String,
        comet_secret: String,
    ) -> anyhow::Result<()> {
        let local_ip = get_local_ip();
        let endpoint = get_endpoint(local_ip.to_string(), mac_addr.clone());
        info!("ssh keepalive current_point {}", endpoint);
        let addr = format!("{}/ssh/register/{}", addr, endpoint);
        let u = addr.parse::<poem::http::Uri>()?;
        let req = ClientRequestBuilder::new(u)
            .with_header("Authorization", format!("Bearer {}", comet_secret));
        let (ws_stream, _b) = timeout(Duration::from_secs(5), connect_async(req))
            .await
            .context("connect timeout")??;

        let (mut sink, mut stream) = ws_stream.split();

        let login_params = loop {
            let next_stream = match timeout(Duration::from_secs(90), stream.next()).await {
                Ok(v) => v,
                Err(e) => {
                    debug!("read stream timeout {e}, retry!");
                    return Ok(());
                }
            };

            match next_stream {
                Some(ret) => {
                    let msg = ret?;
                    if let Message::Text(ready) = msg {
                        let val: SshLoginParams = serde_json::from_str(&ready)?;
                        break val;
                    } else {
                        return Ok(());
                    }
                }
                _ => return Ok(()),
            };
        };

        tokio::spawn(async move {
            let sess = match Session::connect(ConnectParams {
                user: login_params.user,
                password: login_params.password,
                addrs: (local_ip, login_params.port),
            })
            .await
            {
                Ok(v) => v,
                Err(e) => {
                    let _ = sink
                        .send(Message::Text(format!(
                            "\r\n\x1b[31mNotice: failed connect to target server, {e}"
                        )))
                        .await
                        .map_err(|e| error!("failed send message to ws connection - {e}"));
                    return;
                }
            };

            let code = match sess.call2("bash", 20, 30, &mut sink, stream).await {
                Ok(v) => v,
                Err(e) => {
                    let _ = sink
                        .send(Message::Text(format!(
                            "\r\n\x1b[31mNotice: connection closed - {e}"
                        )))
                        .await
                        .map_err(|e| error!("failed send message to ws connection - {e}"));
                    return;
                }
            };

            info!("web ssh exit code {code}");

            if let Err(e) = sess.close().await {
                error!("failed close - {e}");
            }
            info!("ssh tunnel close");
        });

        Ok(())
    }

    pub async fn connect_comet(&mut self) -> anyhow::Result<()> {
        let addr = self.get_comet_addr();
        let local_ip = get_local_ip();

        let mut client = WsClient::new(Some(self.bridge.clone()));

        client
            .set_namespace(self.namespace.clone())
            .set_local_ip(local_ip.clone())
            .set_comet_secret(self.comet_secret.clone())
            .set_mac_address(self.mac_addr.clone())
            .set_initialized(self.is_initialized);

        if let Some(ref opt) = self.assign_user_option {
            client.set_assign_user(opt.to_owned());
        }

        if let Some(ref opt) = self.ssh_connection_option {
            client.set_ssh_connection(opt.to_owned());
        }

        let ws_addr = format!("{}/evt/{}", addr, self.namespace);

        client.connect(&ws_addr, &self.comet_secret).await?;
        let client_key = self.client_key();

        set_comet_addr(addr);

        info!("append new sender {client_key} to {ws_addr}");

        self.bridge
            .append_client(client_key.clone(), client.sender())
            .await;

        self.client.replace(client);
        self.is_initialized = true;
        Ok(())
    }

    async fn exec_job(
        e: Executor,
        react: React,
        schedule_type: Option<ScheduleType>,
        kill_signal_rx: Receiver<()>,
        prev_time: Option<DateTime<Local>>,
        next_time: Option<DateTime<Local>>,
        job_params: &DispatchJobParams,
    ) -> Result<BundleOutput> {
        let start_time = Local::now();
        let schedule_id = job_params.schedule_id.clone();
        let base_job = job_params.base_job.clone();
        let instance_id = job_params.instance_id.to_owned().unwrap();

        let _ = react
            .send_update_job_msg(UpdateJobParams {
                base_job: base_job.to_pure_job(),
                run_status: Some(types::RunStatus::Running),
                schedule_id: schedule_id.clone(),
                next_time,
                prev_time,
                bind_namespace: react.namespace.clone(),
                bind_ip: react.local_ip.clone(),
                schedule_type: schedule_type.clone(),
                created_user: job_params.created_user.clone(),
                run_id: job_params.run_id.clone(),
                start_time: Some(start_time.clone()),
                instance_id: instance_id.clone(),
                ..Default::default()
            })
            .await?;

        let output = match e.run(Ctx { kill_signal_rx }).await {
            Ok(v) => v,
            Err(e) => {
                let bundle_output = if base_job.bundle_script.is_none() {
                    None
                } else {
                    Some(vec![])
                };
                let _ = react
                    .send_update_job_msg(UpdateJobParams {
                        base_job: base_job.to_pure_job(),
                        run_status: Some(types::RunStatus::Stop),
                        schedule_id: schedule_id.clone(),
                        exit_status: Some(e.to_string()),
                        exit_code: Some(99),
                        bind_namespace: react.namespace.clone(),
                        instance_id: instance_id.clone(),
                        bind_ip: react.local_ip.clone(),
                        start_time: Some(start_time),
                        schedule_type: schedule_type.clone(),
                        stdout: Some(e.to_string()),
                        stderr: Some(e.to_string()),
                        end_time: Some(Local::now()),
                        created_user: job_params.created_user.clone(),
                        bundle_output,
                        run_id: job_params.run_id.clone(),
                        ..Default::default()
                    })
                    .await?;
                return Err(e);
            }
        };

        let _ = react
            .send_update_job_msg(UpdateJobParams {
                base_job: base_job.to_pure_job(),
                run_status: Some(types::RunStatus::Stop),
                schedule_id: schedule_id.clone(),
                exit_status: output.get_exit_status(),
                exit_code: output.get_exit_code(),
                instance_id: instance_id.clone(),
                bind_namespace: react.namespace.clone(),
                bind_ip: react.local_ip.clone(),
                start_time: Some(start_time),
                schedule_type: schedule_type.clone(),
                stdout: output.get_stdout(),
                stderr: output.get_stderr(),
                end_time: Some(Local::now()),
                created_user: job_params.created_user.clone(),
                bundle_output: BundleOutputParams::parse(&output),
                run_id: job_params.run_id.clone(),
                ..Default::default()
            })
            .await?;

        Ok(output)
    }

    async fn start_timer(dispatch_params: DispatchJobParams, mut react: React) -> Result<Value> {
        let timer_expr = dispatch_params.timer_expr.clone().unwrap_or_default();
        let base_job = dispatch_params.base_job.clone();
        let pure_job = base_job.to_pure_job();
        let euid = dispatch_params.base_job.eid.clone();
        let react_clone = react.clone();
        let created_user = dispatch_params.created_user.clone();
        let schedule_id = dispatch_params.schedule_id.clone();
        let instance_id = dispatch_params.instance_id.to_owned().unwrap();

        let job: Job = Job::new_cron_job_async_tz(
            timer_expr.as_str(),
            Local,
            move |job_id, mut job_scheduler| {
                let base_job = base_job.clone();
                let (kill_signal_tx, kill_signal_rx) = channel::<()>(1);
                let mut react_clone = react_clone.clone();
                let mut dispatch_params = dispatch_params.clone();
                dispatch_params.run_id = run_id!();

                Box::pin(async move {
                    sleep(Duration::from_millis(10)).await;

                    if let Err(e) = react_clone.can_execute(&dispatch_params).await {
                        error!("ignore execute job - {e}");
                        return;
                    }

                    let next_time = job_scheduler
                        .next_tick_for_job(job_id)
                        .await
                        .unwrap()
                        .map(|v| v.into());

                    let prev_time = Some(Local::now().into());

                    let e = Executor::builder()
                        .job(base_job.clone())
                        .output_dir(react_clone.output_dir.clone())
                        .disable_write_log(true)
                        .build();

                    react_clone
                        .set_execute_context(&dispatch_params, kill_signal_tx)
                        .await;

                    if let Err(e) = Self::exec_job(
                        e,
                        react_clone.clone(),
                        Some(ScheduleType::Timer),
                        kill_signal_rx,
                        prev_time,
                        next_time,
                        &dispatch_params,
                    )
                    .await
                    {
                        error!("failed exec {} - detail: {e}", base_job.eid);
                    }
                    react_clone.end_execute(&dispatch_params).await;
                })
            },
        )
        .map_err(|v| anyhow!("failed parse timer expr {} - {}", timer_expr, v))?;

        let next_time = react.add_job_schedule(euid, job).await?;

        let _ = react
            .send_update_job_msg(UpdateJobParams {
                base_job: pure_job,
                run_status: Some(types::RunStatus::Prepare),
                schedule_status: Some(types::ScheduleStatus::Scheduling),
                schedule_id,
                instance_id,
                exit_status: None,
                exit_code: None,
                stdout: None,
                stderr: None,
                next_time,
                bind_namespace: react.namespace.clone(),
                bind_ip: react.local_ip.clone(),
                schedule_type: Some(ScheduleType::Timer),
                created_user,
                start_time: None,
                ..Default::default()
            })
            .await?;

        Ok(json!(null))
    }

    async fn stop_timer(dispatch_params: DispatchJobParams, mut react: React) -> Result<Value> {
        let instance_id = dispatch_params.instance_id.to_owned().unwrap();
        react
            .remove_job_schedule(&dispatch_params.base_job.eid)
            .await?;
        let _ = react
            .send_update_job_msg(UpdateJobParams {
                base_job: dispatch_params.base_job.to_pure_job(),
                schedule_status: Some(types::ScheduleStatus::Unscheduled),
                run_status: None,
                schedule_id: dispatch_params.schedule_id,
                instance_id,
                exit_status: None,
                exit_code: None,
                stdout: None,
                stderr: None,
                next_time: None,
                bind_namespace: react.namespace.clone(),
                bind_ip: react.local_ip.clone(),
                schedule_type: Some(ScheduleType::Timer),
                created_user: dispatch_params.created_user,
                start_time: None,
                ..Default::default()
            })
            .await?;
        Ok(json!(null))
    }

    async fn start_supervising(
        dispatch_params: DispatchJobParams,
        mut react: React,
    ) -> Result<Value> {
        let eid = dispatch_params.base_job.eid.clone();
        let (tx, mut rx) = unbounded_channel();
        let instance_id = dispatch_params
            .instance_id
            .to_owned()
            .ok_or(anyhow!("not found instance_id in params"))?;
        let _ = react
            .send_update_job_msg(UpdateJobParams {
                base_job: dispatch_params.base_job.to_pure_job(),
                schedule_status: Some(types::ScheduleStatus::Supervising),
                run_status: None,
                schedule_id: dispatch_params.schedule_id.clone(),
                instance_id,
                exit_status: None,
                exit_code: None,
                stdout: None,
                stderr: None,
                next_time: None,
                bind_namespace: react.namespace.clone(),
                bind_ip: react.local_ip.clone(),
                schedule_type: Some(ScheduleType::Daemon),
                created_user: dispatch_params.created_user.clone(),
                start_time: None,
                ..Default::default()
            })
            .await?;

        if !react
            .update_supervising(eid.clone(), dispatch_params.clone(), tx)
            .await
        {
            return Ok(json!(null));
        }

        tokio::spawn(async move {
            let mut dispatch_params = dispatch_params;
            'main: loop {
                let ret = Scheduler::wait_exec(dispatch_params.clone(), react.clone()).await;
                if let Err(e) = ret {
                    error!("supervising: failed exec job - {e}");
                }
                let dur =
                    dispatch_params
                        .restart_interval
                        .clone()
                        .map_or(Duration::from_secs(1), |v| {
                            if v.as_secs() > 0 {
                                v
                            } else {
                                Duration::from_secs(1)
                            }
                        });
                let sleep_time = sleep(dur.to_owned());

                tokio::pin!(sleep_time);

                loop {
                    select! {
                        _ = &mut sleep_time => {
                            info!("supervising: sleep, waiting restart");
                            break;
                        },
                        Some(v) = rx.recv() => {
                            match v {
                                SupervisorSignal::UpdateOptions(opts) => {
                                     dispatch_params = opts.clone();
                                    info!("supervising: update options {:?}", opts);
                                },
                                SupervisorSignal::Exit => {
                                    info!("supervising: exited");
                                    return;
                                }
                            }

                        },
                    }
                }

                continue 'main;
            }
        });
        Ok(json!(null))
    }

    async fn stop_supervising(
        dispatch_params: DispatchJobParams,
        mut react: React,
    ) -> Result<Value> {
        let eid = dispatch_params.base_job.eid.clone();
        react.kill_job(&eid, ScheduleType::Daemon).await;
        react.stop_supervising(&eid).await?;

        let instance_id = dispatch_params
            .instance_id
            .to_owned()
            .ok_or(anyhow!("not found instance_id in params"))?;

        let _ = react
            .send_update_job_msg(UpdateJobParams {
                base_job: dispatch_params.base_job.to_pure_job(),
                schedule_status: Some(types::ScheduleStatus::Unsupervised),
                run_status: None,
                schedule_id: dispatch_params.schedule_id,
                instance_id,
                exit_status: None,
                exit_code: None,
                stdout: None,
                stderr: None,
                next_time: None,
                bind_namespace: react.namespace.clone(),
                bind_ip: react.local_ip.clone(),
                schedule_type: Some(ScheduleType::Daemon),
                created_user: dispatch_params.created_user,
                start_time: None,
                ..Default::default()
            })
            .await?;
        Ok(json!(null))
    }

    async fn restart_supervising(
        dispatch_params: DispatchJobParams,
        react: React,
    ) -> Result<Value> {
        Scheduler::stop_supervising(dispatch_params.clone(), react.clone()).await?;
        Scheduler::start_supervising(dispatch_params.clone(), react).await
    }

    async fn wait_exec(dispatch_params: DispatchJobParams, mut react: React) -> Result<()> {
        let mut base_job = dispatch_params.base_job.clone();
        let (kill_signal_tx, kill_signal_rx) = channel::<()>(1);

        let schedule_type = match dispatch_params.action {
            JobAction::StartSupervising => {
                base_job.timeout = 0;
                ScheduleType::Daemon
            }
            JobAction::Exec => ScheduleType::Once,
            _ => unreachable!(),
        };

        react
            .set_execute_context(&dispatch_params, kill_signal_tx)
            .await;

        let e = Executor::builder()
            .job(base_job.clone())
            .output_dir(react.output_dir.clone())
            .disable_write_log(true)
            .build();

        Self::exec_job(
            e,
            react.clone(),
            Some(schedule_type),
            kill_signal_rx,
            None,
            None,
            &dispatch_params,
        )
        .await?;
        react.end_execute(&dispatch_params).await;

        Ok(())
    }

    async fn exec(dispatch_params: DispatchJobParams, mut react: React) -> Result<Value> {
        let mut base_job = dispatch_params.base_job.clone();
        let (kill_signal_tx, kill_signal_rx) = channel::<()>(1);

        if let Err(e) = react.can_execute(&dispatch_params).await {
            error!("cannot execute job - {e}");
            return Ok(json!(null));
        }

        let schedule_type = match dispatch_params.action {
            JobAction::StartSupervising => {
                base_job.timeout = 0;
                ScheduleType::Daemon
            }
            JobAction::Exec => ScheduleType::Once,
            _ => unreachable!(),
        };

        let e = Executor::builder()
            .job(base_job.clone())
            .output_dir(react.output_dir.clone())
            .disable_write_log(true)
            .build();

        react
            .set_execute_context(&dispatch_params, kill_signal_tx)
            .await;

        if dispatch_params.is_sync {
            let output = Self::exec_job(
                e,
                react.clone(),
                Some(schedule_type),
                kill_signal_rx,
                None,
                None,
                &dispatch_params,
            )
            .await?;
            react.end_execute(&dispatch_params).await;

            return Ok(json!({
                "stdout":output.get_stdout(),
                "exit_code":output.get_exit_code(),
                "stderr":output.get_stderr(),
            }));
        }

        task::spawn(async move {
            if let Err(e) = Self::exec_job(
                e,
                react.clone(),
                Some(schedule_type),
                kill_signal_rx,
                None,
                None,
                &dispatch_params,
            )
            .await
            {
                error!("failed exec {} - detail: {e}", base_job.eid);
            }
            react.end_execute(&dispatch_params).await;
        });

        return Ok(json!(null));
    }

    async fn kill(dispatch_params: DispatchJobParams, mut react: React) -> Result<Value> {
        react
            .kill_job(&dispatch_params.base_job.eid, ScheduleType::Once)
            .await;
        Ok(json!(null))
    }

    pub async fn dispatch_job(dispatch_params: DispatchJobParams, react: React) -> Result<Value> {
        let mut base_job = dispatch_params.base_job.clone();
        let upload_file = base_job.upload_file.take();

        if matches!(
            dispatch_params.action,
            JobAction::StartTimer | JobAction::Exec
        ) {
            if let Some(comet_addr) = get_comet_addr() {
                try_download_file(comet_addr, upload_file).await?;
            }
        }

        match dispatch_params.action {
            JobAction::StartTimer => Scheduler::start_timer(dispatch_params, react).await,
            JobAction::StopTimer => Scheduler::stop_timer(dispatch_params, react).await,
            JobAction::StartSupervising => {
                Scheduler::start_supervising(dispatch_params, react).await
            }
            JobAction::RestartSupervising => {
                Scheduler::restart_supervising(dispatch_params, react).await
            }
            JobAction::StopSupervising => Scheduler::stop_supervising(dispatch_params, react).await,
            JobAction::Exec => Scheduler::exec(dispatch_params, react).await,
            JobAction::Kill => Scheduler::kill(dispatch_params, react).await,
        }
    }

    pub async fn runtime_action(
        action_params: RuntimeActionParams,
        mut react: React,
    ) -> Result<Value> {
        match action_params.action {
            RuntimeAction::StopTimer => react.remove_job_schedule(&action_params.eid).await?,
            RuntimeAction::StopSupervising => react.stop_supervising(&action_params.eid).await?,
            RuntimeAction::Kill => react.kill_job(&action_params.eid, ScheduleType::Once).await,
            _ => unimplemented!(),
        };
        Ok(json!(null))
    }

    pub async fn sftp_read_dir(req: SftpReadDirParams) -> Result<Value> {
        let ret = ssh::read_dir(
            &req.ip,
            req.port,
            &req.user,
            &req.password,
            req.dir.filter(|v| v != "").as_deref(),
        )
        .await?;
        let ret = serde_json::to_value(ret)?;
        Ok(ret)
    }

    pub async fn sftp_upload(req: SftpUploadParams) -> Result<Value> {
        let ret = ssh::upload(
            &req.ip,
            req.port,
            &req.user,
            &req.password,
            &req.filepath,
            req.data,
        )
        .await?;
        let ret = serde_json::to_value(ret)?;
        Ok(ret)
    }

    pub async fn sftp_download(req: SftpDownloadParams) -> Result<Value> {
        let ret = ssh::download(&req.ip, req.port, &req.user, &req.password, &req.filepath).await?;
        let ret = serde_json::to_value(ret)?;
        Ok(ret)
    }

    pub async fn sftp_remove(req: SftpRemoveParams) -> Result<Value> {
        let ret = ssh::remove(
            &req.ip,
            req.port,
            &req.user,
            &req.password,
            &req.remove_type,
            &req.filepath,
        )
        .await?;
        let ret = serde_json::to_value(ret)?;
        Ok(ret)
    }

    pub async fn handle(msg: MsgReqKind, _bridge: Bridge, react: React) -> Value {
        let ret = match msg {
            MsgReqKind::DispatchJobRequest(v) => Self::dispatch_job(v, react.clone()).await,
            MsgReqKind::RuntimeActionRequest(v) => Self::runtime_action(v, react.clone()).await,
            MsgReqKind::SftpReadDirRequest(v) => Self::sftp_read_dir(v).await,
            MsgReqKind::SftpUploadRequest(v) => Self::sftp_upload(v).await,
            MsgReqKind::SftpRemoveRequest(v) => Self::sftp_remove(v).await,
            MsgReqKind::SftpDownloadRequest(v) => Self::sftp_download(v).await,
            MsgReqKind::PullJobRequest(_) => todo!(),
            MsgReqKind::HeartbeatRequest(_) => todo!(),
            _ => todo!(),
        };

        match ret {
            Ok(v) => {
                json!({
                    "code": 20000,
                    "msg": "success",
                    "data": v,
                })
            }
            Err(e) => {
                json!({
                    "code":50000,
                    "msg":e.to_string(),
                })
            }
        }
    }

    pub async fn recv(&mut self, react: React) {
        let bridge = self.bridge.clone();

        while let Some(mut client) = self.client.take() {
            let bridge = bridge.clone();
            let react = react.clone();

            client
                .recv(|msg| async move { Self::handle(msg, bridge, react).await })
                .await;
            client.drop().await;
        }
    }

    pub async fn heartbeat(&self) {
        let bridge = self.bridge.clone();
        let client_key = self.client_key();
        let namespace = self.namespace.clone();
        let source_ip = get_local_ip().to_string();
        let mac_addr = self.mac_addr.clone();
        tokio::spawn(async move {
            loop {
                match bridge
                    .send_msg(
                        &client_key,
                        MsgReqKind::HeartbeatRequest(HeartbeatParams {
                            namespace: namespace.clone(),
                            mac_addr: mac_addr.clone(),
                            source_ip: source_ip.clone(),
                        }),
                    )
                    .await
                {
                    Ok(_v) => {}
                    Err(e) => error!("failed heartbeat {e}, client_key:{client_key}"),
                }
                sleep(Duration::from_secs(60)).await;
                debug!("heartbeat!")
            }
        });
    }

    pub async fn run(&mut self) -> anyhow::Result<()> {
        let react = React::new(
            self.bridge.clone(),
            self.namespace.clone(),
            get_local_ip().to_string(),
            self.client_key(),
            self.output_dir.clone(),
        )
        .await;
        let mut react_clone: React = react.clone();

        self.ssh_poll().await;

        tokio::spawn(async move {
            react_clone
                .start()
                .await
                .expect("failed start cron scheduler");
        });
        self.heartbeat().await;
        loop {
            self.recv(react.clone()).await;
            info!("reconnect after 1s");
            sleep(Duration::from_secs(1)).await;
            if let Err(e) = self.connect_comet().await {
                error!("failed reconnect to comet {:?} - {e}", self.comet_addr);
            }
        }
    }
}
