//! `tunemend`: the command line and MCP server of Tunemend.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rmcp::model::*;
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};
use serde_json::Value;
use tokio::sync::OnceCell;
use tunemend::review_server::{self, SERVER_FILE, SharedService};
use tunemend::service::{Service, Status, ToolResponse, tool_definitions};
use tunemend::store::Store;

const USAGE: &str = "\
tunemend — AI-driven retro game MIDI composition

USAGE:
  tunemend tools                          List tools and what they do
  tunemend schema <tool|bundle>           Print a tool's input schema or the project bundle schema
  tunemend call <tool> [--input FILE]     Run a tool with JSON input (stdin by default); prints the JSON response
  tunemend mcp                            Serve every tool over MCP stdio
  tunemend review [--port N]              Serve the listening screen for the workspace
  tunemend --version

OPTIONS:
  --workspace DIR   Workspace for stored work and outputs (default: current directory)

`call` exits with 0 when the tool succeeded, 2 when it was blocked and 1 on errors.";

const INSTRUCTIONS: &str = include_str!("mcp_instructions.md");

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut workspace = PathBuf::from(".");
    if let Some(i) = args.iter().position(|a| a == "--workspace") {
        if i + 1 >= args.len() {
            fail("--workspace needs a directory");
        }
        workspace = PathBuf::from(args.remove(i + 1));
        args.remove(i);
    }
    let workspace = std::path::absolute(&workspace).unwrap_or_else(|e| fail(&e.to_string()));
    match args.first().map(String::as_str) {
        Some("tools") => {
            for tool in tool_definitions() {
                println!("{:<18} {}", tool.name, tool.description);
            }
        }
        Some("schema") => match args.get(1).map(String::as_str) {
            Some("bundle") => {
                print_json(&serde_json::to_value(tunemend::project_bundle_schema()).unwrap())
            }
            Some(name) => match tool_definitions().into_iter().find(|t| t.name == name) {
                Some(tool) => print_json(&serde_json::to_value(tool.input_schema).unwrap()),
                None => fail(&format!("unknown tool `{name}`")),
            },
            None => fail("schema needs a tool name or `bundle`"),
        },
        Some("call") => {
            let Some(tool) = args.get(1) else {
                fail("call needs a tool name")
            };
            let input = match args
                .iter()
                .position(|a| a == "--input")
                .and_then(|i| args.get(i + 1))
            {
                Some(path) if path != "-" => {
                    std::fs::read_to_string(path).unwrap_or_else(|e| fail(&format!("{path}: {e}")))
                }
                _ => std::io::read_to_string(std::io::stdin())
                    .unwrap_or_else(|e| fail(&e.to_string())),
            };
            let arguments: Value = if input.trim().is_empty() {
                Value::Null
            } else {
                serde_json::from_str(&input)
                    .unwrap_or_else(|e| fail(&format!("input is not JSON: {e}")))
            };
            let mut service = Service::new(&workspace);
            if tool == "present_review" {
                service.set_review_base_url(review_server::running_server_url(&workspace));
            }
            let response = service.call(tool, arguments);
            print_json(&serde_json::to_value(&response).unwrap());
            std::process::exit(match response.status {
                Status::Ok => 0,
                Status::Blocked => 2,
                Status::Error => 1,
            });
        }
        Some("mcp") => runtime().block_on(serve_mcp(workspace)),
        Some("review") => {
            let port = args
                .iter()
                .position(|a| a == "--port")
                .and_then(|i| args.get(i + 1))
                .map(|p| p.parse().unwrap_or_else(|_| fail("--port needs a number")))
                .unwrap_or(0);
            runtime().block_on(serve_review(workspace, port));
        }
        Some("--version" | "-V") => println!("tunemend {}", env!("CARGO_PKG_VERSION")),
        Some("--help" | "-h" | "help") | None => println!("{USAGE}"),
        Some(other) => fail(&format!("unknown command `{other}`\n\n{USAGE}")),
    }
}

fn fail(message: &str) -> ! {
    eprintln!("tunemend: {message}");
    std::process::exit(1);
}

fn print_json(value: &Value) {
    println!("{}", serde_json::to_string_pretty(value).unwrap());
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|e| fail(&e.to_string()))
}

async fn serve_review(workspace: PathBuf, port: u16) {
    if !Store::exists(&workspace) {
        fail("no work is stored in this workspace yet; create a project first");
    }
    let service: SharedService = Arc::new(Mutex::new(Service::new(&workspace)));
    let url = review_server::start(service, port)
        .await
        .unwrap_or_else(|e| fail(&e.to_string()));
    let marker = Store::directory_for(&workspace).join(SERVER_FILE);
    std::fs::write(
        &marker,
        serde_json::json!({"url": url, "pid": std::process::id()}).to_string(),
    )
    .unwrap_or_else(|e| fail(&e.to_string()));
    eprintln!("listening screen at {url} (Ctrl+C to stop)");
    let _ = tokio::signal::ctrl_c().await;
    let _ = std::fs::remove_file(marker);
}

struct ComposerServer {
    service: SharedService,
    review_url: OnceCell<String>,
}

impl ComposerServer {
    /// Starts the listening screen inside this process the first time it is needed.
    async fn review_url(&self) -> Option<String> {
        let workspace = self
            .service
            .lock()
            .expect("service lock")
            .workspace()
            .to_path_buf();
        if let Some(url) = review_server::running_server_url(&workspace) {
            return Some(url);
        }
        self.review_url
            .get_or_try_init(|| review_server::start(self.service.clone(), 0))
            .await
            .ok()
            .cloned()
    }
}

fn mcp_tools() -> Vec<Tool> {
    tool_definitions()
        .into_iter()
        .map(|definition| {
            let schema = serde_json::to_value(definition.input_schema).expect("schema serializes");
            let mut tool = Tool::new(
                definition.name,
                definition.description,
                schema.as_object().cloned().expect("object schema"),
            );
            let mut annotations = ToolAnnotations::new();
            annotations.read_only_hint = Some(definition.read_only);
            annotations.destructive_hint = Some(false);
            annotations.idempotent_hint = Some(true);
            annotations.open_world_hint = Some(false);
            tool.annotations = Some(annotations);
            tool
        })
        .collect()
}

impl ServerHandler for ComposerServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("tunemend", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            tools: mcp_tools(),
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let name = request.name.to_string();
        let arguments = Value::Object(request.arguments.unwrap_or_default());
        if name == "present_review" {
            let url = self.review_url().await;
            self.service
                .lock()
                .expect("service lock")
                .set_review_base_url(url);
        }
        let service = self.service.clone();
        let response: ToolResponse = tokio::task::spawn_blocking(move || {
            service.lock().expect("service lock").call(&name, arguments)
        })
        .await
        .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let data = serde_json::to_value(&response).expect("responses serialize");
        let content = vec![ContentBlock::text(data.to_string())];
        let mut result = if response.status == Status::Ok {
            CallToolResult::success(content)
        } else {
            CallToolResult::error(content)
        };
        result.structured_content = Some(data);
        Ok(result.into())
    }
}

async fn serve_mcp(workspace: PathBuf) {
    let server = ComposerServer {
        service: Arc::new(Mutex::new(Service::new(&workspace))),
        review_url: OnceCell::new(),
    };
    let running = server
        .serve(rmcp::transport::stdio())
        .await
        .unwrap_or_else(|e| fail(&e.to_string()));
    let _ = running.waiting().await;
}
