use crate::{
    store::{get_run_node_on_startup, update_run_node_on_startup},
    types::{AppState, NodeInfo, NodeProcess},
    utils::{
        check_ports_availability, get_binary_path, get_node_ports, get_nodes_dir,
        is_node_process_running, kill_node_process, strip_ansi_escapes,
    },
};
use eyre::{eyre, Result};
use multiaddr::{Multiaddr, Protocol};
use serde_json::Value;
use std::io::BufRead;
use std::io::Write;
use std::{
    fs,
    io::BufReader,
    process::{Command, Stdio},
    sync::{mpsc, Arc, Mutex},
};
use tauri::Manager;
use tauri::State;

pub async fn create_node(
    state: State<'_, AppState>,
    node_name: String,
    server_port: u32,
    swarm_port: u32,
    run_on_startup: bool,
) -> Result<bool, eyre::Error> {
    let nodes_dir = get_nodes_dir(&state.app_handle);
    fs::create_dir_all(&nodes_dir).map_err(|e| eyre!("Failed to create nodes directory: {}", e))?;

    let binary_path = get_binary_path(&state.app_handle)?;
    let output = Command::new(binary_path)
        .args(&[
            "--node-name",
            &node_name,
            "--home",
            nodes_dir
                .to_str()
                .ok_or_else(|| eyre!("Failed to convert path to string"))?,
            "init",
            "--server-port",
            &server_port.to_string(),
            "--swarm-port",
            &swarm_port.to_string(),
        ])
        .output()
        .map_err(|e| eyre!("Failed to execute command: {}", e))?;

    if !output.status.success() {
        return Ok(false);
    }

    update_run_node_on_startup(&state, &node_name, run_on_startup)?;

    Ok(true)
}

pub fn get_nodes(state: State<'_, AppState>) -> Result<Vec<NodeInfo>> {
    let nodes_dir = get_nodes_dir(&state.app_handle);
    let mut nodes = Vec::new();
    for entry in
        fs::read_dir(nodes_dir).map_err(|e| eyre!("Failed to read nodes directory: {}", e))?
    {
        let entry = entry.map_err(|e| eyre!("Failed to read directory entry: {}", e))?;
        if let Some(node_name) = entry.file_name().to_str() {
            let node_name = node_name.to_owned();
            if let Ok(config) = get_node_ports(&node_name, &state.app_handle) {
                let is_running = is_node_process_running(&node_name)?;
                let run_on_startup = get_run_node_on_startup(&state, &node_name)?;
                nodes.push(NodeInfo {
                    name: node_name,
                    is_running,
                    run_on_startup,
                    node_ports: config,
                });
            }
        }
    }

    Ok(nodes)
}

pub async fn update_node_config(
    state: State<'_, AppState>,
    original_node_name: String,
    node_name: String,
    server_port: u32,
    swarm_port: u32,
    run_on_startup: bool,
) -> Result<bool> {
    let nodes_dir = get_nodes_dir(&state.app_handle);
    let original_node_dir = nodes_dir.join(&original_node_name);
    let new_node_dir = nodes_dir.join(&node_name);

    // Check if the new node name is already taken
    if original_node_name != node_name && new_node_dir.exists() {
        return Err(eyre!(
            "Cannot change node name, node with name {} already exists",
            node_name
        ));
    }

    // Read the config file
    let config_path = original_node_dir.join("config.toml");
    let config_content =
        fs::read_to_string(&config_path).map_err(|e| eyre!("Failed to read config file: {}", e))?;

    let mut config: Value =
        toml::from_str(&config_content).map_err(|e| eyre!("Failed to parse config file: {}", e))?;

    // Update the "swarm" and "server" sections
    update_port(&mut config, "swarm", &swarm_port.to_string())
        .map_err(|e| eyre!("Failed to update swarm port: {}", e))?;
    update_port(&mut config, "server", &server_port.to_string())
        .map_err(|e| eyre!("Failed to update server port: {}", e))?;

    // Serialize the updated config back to TOML
    let updated_content =
        toml::to_string(&config).map_err(|e| eyre!("Failed to serialize config: {}", e))?;

    // Write the updated content back to the file
    fs::write(&config_path, updated_content)
        .map_err(|e| eyre!("Failed to write updated config: {}", e))?;

    // Rename the node directory if the name has changed
    if original_node_name != node_name {
        fs::rename(&original_node_dir, &new_node_dir)
            .map_err(|e| eyre!("Failed to rename node directory: {}", e))?;
    }

    update_run_node_on_startup(&state, &node_name, run_on_startup)
        .map_err(|e| eyre!("Failed to update option to run node on startup: {}", e))?;

    Ok(true)
}

// Helper function to update port numbers
fn update_port(config: &mut Value, section: &str, new_port: &str) -> Result<()> {
    if let Some(listen) = config[section]["listen"].as_array_mut() {
        for entry in listen.iter_mut() {
            if let Some(s) = entry.as_str() {
                let new_value = change_port_in_path(s, new_port)?;
                *entry = Value::String(new_value);
            }
        }
    }
    Ok(())
}

// Helper function to change the port in the path-like string
fn change_port_in_path(address: &str, new_port: &str) -> Result<String> {
    // Parse the address into a Multiaddr
    let multiaddr: Multiaddr = address
        .parse()
        .map_err(|e| eyre!("Invalid multiaddr format: {}", e))?;

    // Iterate over the protocols and replace the port
    let mut new_multiaddr = Multiaddr::empty();
    for protocol in multiaddr.iter() {
        match protocol {
            Protocol::Tcp(_) => {
                let port = new_port
                    .parse()
                    .map_err(|e| eyre!("Invalid port number: {}", e))?;
                new_multiaddr.push(Protocol::Tcp(port));
            }
            Protocol::Udp(_) => {
                let port = new_port
                    .parse()
                    .map_err(|e| eyre!("Invalid port number: {}", e))?;
                new_multiaddr.push(Protocol::Udp(port));
            }
            _ => new_multiaddr.push(protocol),
        }
    }

    // Convert the modified Multiaddr back to a string
    Ok(new_multiaddr.to_string())
}

// Start a node process
pub async fn start_node(state: State<'_, AppState>, node_name: String) -> Result<bool> {
    let app_handle = state.app_handle.clone();
    let node_manager = state.node_manager.clone();

    let mut manager = node_manager
        .lock()
        .map_err(|e| eyre!("Failed to lock node manager: {}", e))?;

    if manager.nodes.contains_key(&node_name) {
        return Ok(false);
    }

    let config = get_node_ports(&node_name, &app_handle)?;
    check_ports_availability(&config)?;

    let nodes_dir = get_nodes_dir(&app_handle);
    let binary_path = get_binary_path(&app_handle)?;

    let mut process = Command::new(binary_path)
        .args(&[
            "--node-name",
            &node_name,
            "--home",
            nodes_dir
                .to_str()
                .ok_or_else(|| eyre!("Failed to convert path to string".to_string()))?,
            "run",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| eyre!("Failed to spawn node process: {}", e))?;

    let (tx, rx) = mpsc::channel::<String>();
    let output = Arc::new(Mutex::new(String::new()));

    let stdin = process
        .stdin
        .take()
        .ok_or_else(|| eyre!("Failed to capture stdin".to_string()))?;
    let stdout = process
        .stdout
        .take()
        .ok_or_else(|| eyre!("Failed to capture stdout".to_string()))?;
    let stderr = process
        .stderr
        .take()
        .ok_or_else(|| eyre!("Failed to capture stderr".to_string()))?;

    // Spawn a  thread to handle stdin
    std::thread::spawn({
        let node_name = node_name.clone();
        move || {
            let mut stdin = stdin;
            for input in rx {
                if writeln!(stdin, "{}", input).is_err() {
                    eprintln!("Failed to write to stdin for node: {}", node_name);
                    break;
                }
            }
        }
    });

    // Spawn a thread to handle stdout and stderr
    std::thread::spawn({
        let output = Arc::clone(&output);
        let node_name = node_name.clone();
        let app_handle = app_handle.clone();

        move || {
            let stdout_reader = BufReader::new(stdout);
            let stderr_reader = BufReader::new(stderr);

            let process_line = |line: std::io::Result<String>| -> Result<()> {
                let l = line.map_err(|e| eyre!("Failed to read line: {}", e))?;
                let cleaned_line = strip_ansi_escapes(&l);

                {
                    let mut output_lock = output
                        .lock()
                        .map_err(|e| eyre!("Failed to lock output: {}", e))?;
                    output_lock.push_str(&cleaned_line);
                    output_lock.push('\n');
                }
                // Ensure the emitted event name and data format are correct
                app_handle
                    .emit_all(&format!("node-output-{}", node_name), cleaned_line + "\n")
                    .map_err(|e| eyre!("Failed to emit output for node {}: {}", node_name, e))?;

                Ok(())
            };

            for line in stdout_reader.lines().chain(stderr_reader.lines()) {
                if let Err(e) = process_line(line) {
                    eprintln!("Error processing line for node {}: {}", node_name, e);
                    return Err(e);
                }
            }
            Ok(())
        }
    });

    let node_process = NodeProcess {
        process: Some(process),
        stdin: Some(tx),
        output,
    };
    manager.nodes.insert(node_name.clone(), node_process);

    Ok(true)
}

pub fn get_node_output(state: State<'_, AppState>, node_name: String) -> Result<String> {
    let manager = state
        .node_manager
        .lock()
        .map_err(|e| eyre!("Failed to lock node manager: {}", e))?;

    match manager.nodes.get(&node_name) {
        Some(node_process) => {
            let output = node_process
                .output
                .lock()
                .map_err(|e| eyre!("Failed to lock output: {}", e))?
                .clone();

            Ok(output)
        }
        None => Err(eyre!("Node not found: {}", node_name)),
    }
}

pub async fn stop_node_process(state: State<'_, AppState>, node_name: String) -> Result<bool> {
    if !is_node_process_running(&node_name)? {
        return Ok(false);
    }

    let node_stopped = 'done: {
        let mut manager = state
            .node_manager
            .lock()
            .map_err(|e| eyre!("Failed to lock node manager: {}", e))?;
        if let Some(mut node_process) = manager.nodes.remove(&node_name) {
            if let Some(mut process) = node_process.process.take() {
                break 'done process.kill().is_ok();
            }
        }
        false
    };

    if !node_stopped {
        kill_node_process(&node_name).map_err(|e| eyre!("Failed to stop node: {}", e))?;
    }

    Ok(true)
}

pub fn send_input_to_node(
    node_name: String,
    input: String,
    state: State<'_, AppState>,
) -> Result<bool> {
    let manager = state
        .node_manager
        .lock()
        .map_err(|e| eyre!("Failed to lock node manager: {}", e))?;
    let node_process = manager
        .nodes
        .get(&node_name)
        .ok_or_else(|| eyre!("Node not found: {}", node_name))?;

    let stdin = node_process
        .stdin
        .as_ref()
        .ok_or_else(|| eyre!("Node is not running: {}", node_name))?;

    stdin
        .send(input)
        .map_err(|e| eyre!("Failed to send input: {}", e))?;
    Ok(true)
}
