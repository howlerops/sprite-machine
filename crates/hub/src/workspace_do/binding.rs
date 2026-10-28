//! The `WORKSPACE` namespace binding (wasm32 only, M0-008b): the hub calls the Workspace DO
//! `POST /rpc/<method>` on the object named by the workspace id. The binding is declared in
//! `crates/hub/wrangler.toml` with `script_name` (the `sm-workspace` Worker, `workers/workspace`).

use serde::{Serialize, de::DeserializeOwned};
use worker::wasm_bindgen::JsValue;
use worker::{Env, Headers, Method, Request, RequestInit};

use super::{
    BackupRequest, BackupResponse, ErrorCode, ExecRequest, ExecResponse, PreviewRequest,
    PreviewResponse, RestoreBackupRequest, RestoreBackupResponse, RestoreRequest, RestoreResponse,
    SnapshotRequest, SnapshotResponse, StartRequest, StartResponse, StatusRequest, StatusResponse,
    StopRequest, StopResponse, WorkspaceControl, WorkspaceError, decode_response,
};

/// The binding name in `crates/hub/wrangler.toml`.
pub const WORKSPACE_BINDING: &str = "WORKSPACE";

/// Host of the request sent to the DO stub. The DO routes on the path only, so any origin works.
const WORKSPACE_ORIGIN: &str = "https://workspace.internal";

/// One workspace, reached through the `WORKSPACE` namespace.
pub struct WorkspaceDoBinding<'a> {
    env: &'a Env,
    workspace_id: &'a str,
}

impl<'a> WorkspaceDoBinding<'a> {
    pub fn new(env: &'a Env, workspace_id: &'a str) -> Self {
        Self { env, workspace_id }
    }

    /// `POST /rpc/<method>` with a JSON body, returning the HTTP status and the body as sent.
    /// The dev-fixture route passes this through unchanged.
    pub async fn post_raw(
        &self,
        method: &str,
        body: &str,
    ) -> Result<(u16, String), WorkspaceError> {
        let transport = |e: worker::Error| {
            WorkspaceError::new(
                ErrorCode::Internal,
                format!("workspace DO call failed: {e}"),
            )
        };
        let namespace = self
            .env
            .durable_object(WORKSPACE_BINDING)
            .map_err(transport)?;
        let stub = namespace
            .id_from_name(self.workspace_id)
            .map_err(transport)?
            .get_stub()
            .map_err(transport)?;

        let headers = Headers::new();
        headers
            .set("content-type", "application/json")
            .map_err(transport)?;
        let mut init = RequestInit::new();
        init.with_method(Method::Post)
            .with_headers(headers)
            .with_body(Some(JsValue::from_str(body)));
        let url = format!("{WORKSPACE_ORIGIN}/rpc/{method}");
        let request = Request::new_with_init(&url, &init).map_err(transport)?;

        let mut response = stub.fetch_with_request(request).await.map_err(transport)?;
        let status = response.status_code();
        let text = response.text().await.map_err(transport)?;
        Ok((status, text))
    }

    async fn call<Req, Resp>(&self, method: &str, req: &Req) -> Result<Resp, WorkspaceError>
    where
        Req: Serialize,
        Resp: DeserializeOwned,
    {
        let body = serde_json::to_string(req)
            .map_err(|e| WorkspaceError::new(ErrorCode::Internal, e.to_string()))?;
        let (status, text) = self.post_raw(method, &body).await?;
        decode_response(status, &text)
    }
}

impl WorkspaceControl for WorkspaceDoBinding<'_> {
    async fn start(&self, req: StartRequest) -> Result<StartResponse, WorkspaceError> {
        self.call("start", &req).await
    }

    async fn stop(&self, req: StopRequest) -> Result<StopResponse, WorkspaceError> {
        self.call("stop", &req).await
    }

    async fn status(&self, req: StatusRequest) -> Result<StatusResponse, WorkspaceError> {
        self.call("status", &req).await
    }

    async fn snapshot(&self, req: SnapshotRequest) -> Result<SnapshotResponse, WorkspaceError> {
        self.call("snapshot", &req).await
    }

    async fn restore(&self, req: RestoreRequest) -> Result<RestoreResponse, WorkspaceError> {
        self.call("restore", &req).await
    }

    async fn backup(&self, req: BackupRequest) -> Result<BackupResponse, WorkspaceError> {
        self.call("backup", &req).await
    }

    async fn restore_backup(
        &self,
        req: RestoreBackupRequest,
    ) -> Result<RestoreBackupResponse, WorkspaceError> {
        self.call("restoreBackup", &req).await
    }

    async fn exec(&self, req: ExecRequest) -> Result<ExecResponse, WorkspaceError> {
        self.call("exec", &req).await
    }

    async fn preview(&self, req: PreviewRequest) -> Result<PreviewResponse, WorkspaceError> {
        self.call("preview", &req).await
    }
}
