use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;
use std::sync::mpsc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rayon::ThreadPoolBuilder;
use rayon::prelude::*;

use crate::config::Config;
use crate::model::{GpuInfo, GpuProcessInfo, HostTree, HostUpdate, PaneInfo, ProcessInfo};

struct PaneSnapshot {
    panes: Vec<PaneInfo>,
    processes: Vec<ProcessInfo>,
}

pub(crate) fn collect_hosts(config: &Config) -> Vec<HostTree> {
    let pool = ThreadPoolBuilder::new()
        .num_threads(config.scan_concurrency)
        .build();

    match pool {
        Ok(pool) => pool.install(|| {
            config
                .hosts
                .par_iter()
                .map(|host| collect_host_snapshot(host, config.connect_timeout_secs))
                .collect()
        }),
        Err(_) => config
            .hosts
            .iter()
            .map(|host| collect_host_snapshot(host, config.connect_timeout_secs))
            .collect(),
    }
}

pub(crate) fn collect_pane_updates_streaming(
    config: &Config,
    hosts: &[String],
    sender: mpsc::Sender<HostUpdate>,
) {
    let pool = ThreadPoolBuilder::new()
        .num_threads(config.scan_concurrency)
        .build();

    match pool {
        Ok(pool) => pool.install(|| {
            hosts.par_iter().for_each(|host| {
                let _ = sender.send(collect_pane_update(host, config.connect_timeout_secs));
            });
        }),
        Err(_) => {
            for host in hosts {
                let _ = sender.send(collect_pane_update(host, config.connect_timeout_secs));
            }
        }
    }
}

pub(crate) fn collect_gpu_updates_streaming(
    config: &Config,
    hosts: &[String],
    sender: mpsc::Sender<HostUpdate>,
) {
    let pool = ThreadPoolBuilder::new()
        .num_threads(config.scan_concurrency)
        .build();

    match pool {
        Ok(pool) => pool.install(|| {
            hosts.par_iter().for_each(|host| {
                let _ = sender.send(collect_gpu_update(host, config.connect_timeout_secs));
            });
        }),
        Err(_) => {
            for host in hosts {
                let _ = sender.send(collect_gpu_update(host, config.connect_timeout_secs));
            }
        }
    }
}

fn collect_host_snapshot(host: &str, connect_timeout_secs: u64) -> HostTree {
    match list_remote_panes(host, connect_timeout_secs) {
        Ok(snapshot) => {
            let (gpus, gpu_processes) =
                collect_remote_gpus(host, connect_timeout_secs).unwrap_or_default();
            let mut tree = HostTree {
                host: host.to_string(),
                panes: snapshot.panes,
                processes: snapshot.processes,
                gpus,
                gpu_processes,
                error: None,
                connecting: false,
            };
            mark_pane_gpu_indices(
                &mut tree.panes,
                &tree.processes,
                &tree.gpus,
                &tree.gpu_processes,
            );
            tree
        }
        Err(err) => HostTree {
            host: host.to_string(),
            panes: Vec::new(),
            processes: Vec::new(),
            gpus: Vec::new(),
            gpu_processes: Vec::new(),
            error: Some(err.to_string()),
            connecting: false,
        },
    }
}

fn collect_pane_update(host: &str, connect_timeout_secs: u64) -> HostUpdate {
    match list_remote_panes(host, connect_timeout_secs) {
        Ok(snapshot) => HostUpdate::Panes {
            host: host.to_string(),
            panes: snapshot.panes,
            processes: snapshot.processes,
            error: None,
        },
        Err(err) => HostUpdate::Panes {
            host: host.to_string(),
            panes: Vec::new(),
            processes: Vec::new(),
            error: Some(err.to_string()),
        },
    }
}

fn collect_gpu_update(host: &str, connect_timeout_secs: u64) -> HostUpdate {
    let (gpus, gpu_processes) = collect_remote_gpus(host, connect_timeout_secs).unwrap_or_default();
    HostUpdate::Gpus {
        host: host.to_string(),
        gpus,
        gpu_processes,
    }
}

pub(crate) fn sort_trees_by_config(trees: &mut [HostTree], hosts: &[String]) {
    let order: BTreeMap<&str, usize> = hosts
        .iter()
        .enumerate()
        .map(|(index, host)| (host.as_str(), index))
        .collect();
    trees.sort_by_key(|tree| order.get(tree.host.as_str()).copied().unwrap_or(usize::MAX));
}

fn list_remote_panes(host: &str, connect_timeout_secs: u64) -> Result<PaneSnapshot> {
    let format = [
        "#{session_name}",
        "#{session_id}",
        "#{session_created}",
        "#{window_index}",
        "#{window_id}",
        "#{window_name}",
        "#{pane_index}",
        "#{pane_id}",
        "#{pane_pid}",
        "#{pane_current_command}",
        "#{pane_current_path}",
        "#{pane_title}",
        "#{window_active}",
        "#{pane_active}",
    ]
    .join("\t");

    let remote_command = format!(
        "printf '%s\\n' __TMUX_GATEWAY_PANES__; tmux list-panes -a -F {}; printf '%s\\n' __TMUX_GATEWAY_PROCESSES__; ps -eo pid=,ppid=,etimes=,comm=,args= -ww 2>/dev/null || true",
        shell_quote(&format),
    );
    let output = Command::new("ssh")
        .args(ssh_options(connect_timeout_secs))
        .arg(host)
        .arg(remote_command)
        .output()
        .with_context(|| format!("failed to start ssh for host {host}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.contains("no server running") || stderr.contains("No such file or directory") {
            return Ok(PaneSnapshot {
                panes: Vec::new(),
                processes: Vec::new(),
            });
        }
        bail!(
            "ssh/tmux command failed for host {host}: {}",
            if stderr.is_empty() {
                output.status.to_string()
            } else {
                stderr
            }
        );
    }

    let stdout = String::from_utf8(output.stdout)
        .with_context(|| format!("tmux output from host {host} was not utf-8"))?;
    let mut snapshot = parse_remote_snapshot(&stdout)
        .with_context(|| format!("failed to parse tmux panes from host {host}"))?;

    mark_created_times(&mut snapshot.panes, &snapshot.processes);
    let process_by_pid = process_by_pid(&snapshot.processes);
    let children_by_parent = children_by_parent(&snapshot.processes);
    mark_busy_panes(&mut snapshot.panes, &process_by_pid, &children_by_parent);

    if let Ok(cwd_by_pid) = collect_remote_process_cwds(
        host,
        connect_timeout_secs,
        pane_display_pids(&snapshot.panes, &process_by_pid, &children_by_parent),
    ) {
        apply_pane_command_cwds(
            &mut snapshot.panes,
            &cwd_by_pid,
            &process_by_pid,
            &children_by_parent,
        );
    }

    Ok(snapshot)
}

fn parse_remote_snapshot(output: &str) -> Result<PaneSnapshot> {
    let mut pane_lines = Vec::new();
    let mut process_lines = Vec::new();
    let mut section = "";

    for line in output.lines() {
        match line {
            "__TMUX_GATEWAY_PANES__" => {
                section = "panes";
                continue;
            }
            "__TMUX_GATEWAY_PROCESSES__" => {
                section = "processes";
                continue;
            }
            _ => {}
        }

        match section {
            "panes" => pane_lines.push(line),
            "processes" => process_lines.push(line),
            _ => {}
        }
    }

    let panes = parse_panes(&pane_lines.join("\n"))?;
    let processes = parse_processes(&process_lines.join("\n"));
    Ok(PaneSnapshot { panes, processes })
}

pub(crate) fn parse_panes(output: &str) -> Result<Vec<PaneInfo>> {
    let mut panes = Vec::new();

    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() != 14 {
            bail!(
                "expected 14 tab-separated fields, got {} in line {line:?}",
                fields.len()
            );
        }

        panes.push(PaneInfo {
            session_name: fields[0].to_string(),
            session_id: fields[1].to_string(),
            session_created: parse_created_epoch(fields[2]),
            window_index: fields[3].to_string(),
            window_id: fields[4].to_string(),
            window_created: None,
            window_name: fields[5].to_string(),
            pane_index: fields[6].to_string(),
            pane_id: fields[7].to_string(),
            pane_created: None,
            pane_pid: fields[8].parse().unwrap_or(0),
            pane_current_command: fields[9].to_string(),
            pane_commandline: fields[9].to_string(),
            pane_current_path: fields[10].to_string(),
            pane_command_cwd: fields[10].to_string(),
            pane_title: fields[11].to_string(),
            active_window: fields[12] == "1",
            active_pane: fields[13] == "1",
            busy_duration_secs: None,
            gpu_indices: Vec::new(),
            gpu_memory_by_index: Vec::new(),
        });
    }

    Ok(panes)
}

fn parse_created_epoch(value: &str) -> Option<u64> {
    value.parse::<u64>().ok().filter(|seconds| *seconds > 0)
}

fn mark_created_times(panes: &mut [PaneInfo], processes: &[ProcessInfo]) {
    mark_created_times_at(panes, processes, current_unix_epoch());
}

pub(crate) fn mark_created_times_at(
    panes: &mut [PaneInfo],
    processes: &[ProcessInfo],
    now_epoch: u64,
) {
    let process_by_pid: BTreeMap<u32, &ProcessInfo> = processes
        .iter()
        .map(|process| (process.pid, process))
        .collect();

    for pane in panes.iter_mut() {
        pane.pane_created = process_by_pid
            .get(&pane.pane_pid)
            .map(|process| now_epoch.saturating_sub(process.elapsed_secs));
    }

    let mut window_created_by_key: BTreeMap<(String, String), u64> = BTreeMap::new();
    for pane in panes.iter().filter(|pane| pane.pane_created.is_some()) {
        let key = (pane.session_name.clone(), pane.window_index.clone());
        let pane_created = pane.pane_created.unwrap_or_default();
        window_created_by_key
            .entry(key)
            .and_modify(|window_created| *window_created = (*window_created).min(pane_created))
            .or_insert(pane_created);
    }

    for pane in panes {
        pane.window_created = window_created_by_key
            .get(&(pane.session_name.clone(), pane.window_index.clone()))
            .copied();
    }
}

fn current_unix_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn collect_remote_gpus(
    host: &str,
    connect_timeout_secs: u64,
) -> Result<(Vec<GpuInfo>, Vec<GpuProcessInfo>)> {
    let remote_command = "printf '%s\\n' __TMUX_GATEWAY_GPUS__; nvidia-smi --query-gpu=index,uuid,memory.used,memory.total --format=csv,noheader,nounits 2>/dev/null || true; printf '%s\\n' __TMUX_GATEWAY_GPU_PROCESSES__; nvidia-smi --query-compute-apps=gpu_uuid,pid,used_memory --format=csv,noheader,nounits 2>/dev/null || true";
    parse_gpu_snapshot(&run_remote_optional(
        host,
        connect_timeout_secs,
        remote_command,
    )?)
}

fn run_remote_optional(
    host: &str,
    connect_timeout_secs: u64,
    remote_command: &str,
) -> Result<String> {
    let output = Command::new("ssh")
        .args(ssh_options(connect_timeout_secs))
        .arg(host)
        .arg(remote_command)
        .output()
        .with_context(|| format!("failed to start ssh for host {host}"))?;

    if !output.status.success() {
        return Ok(String::new());
    }

    String::from_utf8(output.stdout)
        .with_context(|| format!("nvidia-smi output from host {host} was not utf-8"))
}

pub(crate) fn parse_gpus(output: &str) -> Vec<GpuInfo> {
    output
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(',').map(str::trim).collect();
            if fields.len() < 4 {
                return None;
            }
            Some(GpuInfo {
                index: fields[0].parse().ok()?,
                uuid: fields[1].to_string(),
                memory_used_mib: fields[2].parse().ok()?,
                memory_total_mib: fields[3].parse().ok()?,
            })
        })
        .collect()
}

pub(crate) fn parse_gpu_processes(output: &str) -> Vec<GpuProcessInfo> {
    output
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(',').map(str::trim).collect();
            if fields.len() < 2 {
                return None;
            }
            Some(GpuProcessInfo {
                gpu_uuid: fields[0].to_string(),
                pid: fields[1].parse().ok()?,
                used_memory_mib: fields
                    .get(2)
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0),
            })
        })
        .collect()
}

pub(crate) fn parse_gpu_snapshot(output: &str) -> Result<(Vec<GpuInfo>, Vec<GpuProcessInfo>)> {
    let mut gpu_lines = Vec::new();
    let mut process_lines = Vec::new();
    let mut section = "";

    for line in output.lines() {
        match line {
            "__TMUX_GATEWAY_GPUS__" => {
                section = "gpus";
                continue;
            }
            "__TMUX_GATEWAY_GPU_PROCESSES__" => {
                section = "processes";
                continue;
            }
            _ => {}
        }

        match section {
            "gpus" => gpu_lines.push(line),
            "processes" => process_lines.push(line),
            _ => {}
        }
    }

    Ok((
        parse_gpus(&gpu_lines.join("\n")),
        parse_gpu_processes(&process_lines.join("\n")),
    ))
}

pub(crate) fn parse_processes(output: &str) -> Vec<ProcessInfo> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pid = parts.next()?.parse().ok()?;
            let ppid = parts.next()?.parse().ok()?;
            let elapsed_secs = parts.next()?.parse().ok()?;
            let command = parts.next()?.to_string();
            let commandline = parts.collect::<Vec<_>>().join(" ");
            Some(ProcessInfo {
                pid,
                ppid,
                elapsed_secs,
                command,
                commandline,
            })
        })
        .collect()
}

fn mark_busy_panes(
    panes: &mut [PaneInfo],
    process_by_pid: &BTreeMap<u32, &ProcessInfo>,
    children_by_parent: &BTreeMap<u32, Vec<&ProcessInfo>>,
) {
    for pane in panes {
        if let Some(process) = pane_display_process(pane, process_by_pid, children_by_parent) {
            pane.pane_commandline = process.commandline.clone();
        }
        pane.busy_duration_secs = pane_busy_duration(pane, process_by_pid, children_by_parent);
    }
}

fn collect_remote_process_cwds(
    host: &str,
    connect_timeout_secs: u64,
    pids: BTreeSet<u32>,
) -> Result<BTreeMap<u32, String>> {
    if pids.is_empty() {
        return Ok(BTreeMap::new());
    }

    let pid_args = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(" ");
    let remote_command = format!(
        "if command -v pwdx >/dev/null 2>&1; then pwdx {pid_args} 2>/dev/null || true; else for pid in {pid_args}; do cwd=$(readlink \"/proc/$pid/cwd\" 2>/dev/null || true); printf '%s: %s\\n' \"$pid\" \"$cwd\"; done; fi",
    );
    Ok(parse_process_cwds(&run_remote_optional(
        host,
        connect_timeout_secs,
        &remote_command,
    )?))
}

pub(crate) fn parse_process_cwds(output: &str) -> BTreeMap<u32, String> {
    output
        .lines()
        .filter_map(|line| {
            let (pid, cwd) = line.split_once(':')?;
            Some((pid.trim().parse().ok()?, cwd.trim_start().to_string()))
        })
        .collect()
}

fn apply_pane_command_cwds(
    panes: &mut [PaneInfo],
    cwd_by_pid: &BTreeMap<u32, String>,
    process_by_pid: &BTreeMap<u32, &ProcessInfo>,
    children_by_parent: &BTreeMap<u32, Vec<&ProcessInfo>>,
) {
    for pane in panes {
        let Some(process) = pane_display_process(pane, process_by_pid, children_by_parent) else {
            continue;
        };
        let Some(cwd) = cwd_by_pid.get(&process.pid) else {
            continue;
        };
        if !cwd.is_empty() {
            pane.pane_command_cwd = cwd.clone();
        }
    }
}

pub(crate) fn mark_pane_gpu_indices(
    panes: &mut [PaneInfo],
    processes: &[ProcessInfo],
    gpus: &[GpuInfo],
    gpu_processes: &[GpuProcessInfo],
) {
    let gpu_index_by_uuid: BTreeMap<&str, usize> = gpus
        .iter()
        .map(|gpu| (gpu.uuid.as_str(), gpu.index))
        .collect();
    let mut children_by_parent: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for process in processes {
        children_by_parent
            .entry(process.ppid)
            .or_default()
            .push(process.pid);
    }

    for pane in panes {
        let process_tree = pane_process_tree(pane.pane_pid, &children_by_parent);
        let mut memory_by_index: BTreeMap<usize, u64> = BTreeMap::new();
        for gpu_process in gpu_processes
            .iter()
            .filter(|gpu_process| process_tree.contains(&gpu_process.pid))
        {
            let Some(index) = gpu_index_by_uuid.get(gpu_process.gpu_uuid.as_str()) else {
                continue;
            };
            *memory_by_index.entry(*index).or_default() += gpu_process.used_memory_mib;
        }
        pane.gpu_indices = memory_by_index.keys().copied().collect();
        pane.gpu_memory_by_index = memory_by_index.into_iter().collect();
    }
}

fn pane_process_tree(root_pid: u32, children_by_parent: &BTreeMap<u32, Vec<u32>>) -> BTreeSet<u32> {
    let mut process_tree = BTreeSet::new();
    if root_pid == 0 {
        return process_tree;
    }

    let mut stack = vec![root_pid];
    while let Some(pid) = stack.pop() {
        if !process_tree.insert(pid) {
            continue;
        }
        if let Some(children) = children_by_parent.get(&pid) {
            stack.extend(children.iter().copied());
        }
    }

    process_tree
}

fn pane_running_process<'a>(
    pane: &PaneInfo,
    process_by_pid: &BTreeMap<u32, &'a ProcessInfo>,
    children_by_parent: &BTreeMap<u32, Vec<&'a ProcessInfo>>,
) -> Option<&'a ProcessInfo> {
    if pane.pane_pid == 0 {
        return None;
    }

    if let Some(process) = process_by_pid.get(&pane.pane_pid) {
        if !is_shell_command(&process.command) {
            return Some(process);
        }
    }

    if !is_shell_command(&pane.pane_current_command) {
        return None;
    }

    let mut best: Option<&ProcessInfo> = None;
    let mut stack = children_by_parent
        .get(&pane.pane_pid)
        .cloned()
        .unwrap_or_default();
    while let Some(process) = stack.pop() {
        if !is_shell_command(&process.command) {
            match &best {
                Some(best_process) if best_process.elapsed_secs >= process.elapsed_secs => {}
                _ => best = Some(process),
            }
        }
        if let Some(children) = children_by_parent.get(&process.pid) {
            stack.extend(children.iter().copied());
        }
    }

    best
}

fn pane_display_process<'a>(
    pane: &PaneInfo,
    process_by_pid: &BTreeMap<u32, &'a ProcessInfo>,
    children_by_parent: &BTreeMap<u32, Vec<&'a ProcessInfo>>,
) -> Option<&'a ProcessInfo> {
    pane_running_process(pane, process_by_pid, children_by_parent)
        .or_else(|| process_by_pid.get(&pane.pane_pid).copied())
}

fn pane_display_pids(
    panes: &[PaneInfo],
    process_by_pid: &BTreeMap<u32, &ProcessInfo>,
    children_by_parent: &BTreeMap<u32, Vec<&ProcessInfo>>,
) -> BTreeSet<u32> {
    panes.iter()
        .filter_map(|pane| pane_display_process(pane, process_by_pid, children_by_parent))
        .map(|process| process.pid)
        .collect()
}

pub(crate) fn pane_busy_duration(
    pane: &PaneInfo,
    process_by_pid: &BTreeMap<u32, &ProcessInfo>,
    children_by_parent: &BTreeMap<u32, Vec<&ProcessInfo>>,
) -> Option<u64> {
    pane_running_process(pane, process_by_pid, children_by_parent).map(|process| process.elapsed_secs)
}

fn process_by_pid(processes: &[ProcessInfo]) -> BTreeMap<u32, &ProcessInfo> {
    processes
        .iter()
        .map(|process| (process.pid, process))
        .collect()
}

fn children_by_parent(processes: &[ProcessInfo]) -> BTreeMap<u32, Vec<&ProcessInfo>> {
    let mut children_by_parent = BTreeMap::new();
    for process in processes {
        children_by_parent
            .entry(process.ppid)
            .or_insert_with(Vec::new)
            .push(process);
    }
    children_by_parent
}

fn is_shell_command(command: &str) -> bool {
    let command = command.rsplit('/').next().unwrap_or(command);
    matches!(
        command,
        "sh" | "bash"
            | "zsh"
            | "fish"
            | "dash"
            | "ksh"
            | "mksh"
            | "tcsh"
            | "csh"
            | "pwsh"
            | "powershell"
    )
}

pub(crate) fn ssh_options(connect_timeout_secs: u64) -> Vec<String> {
    vec![
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        format!("ConnectTimeout={connect_timeout_secs}"),
    ]
}

pub(crate) fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }

    let mut quoted = String::from("'");
    for ch in value.chars() {
        if ch == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}
