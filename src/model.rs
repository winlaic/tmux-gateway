#[derive(Clone, Debug)]
pub(crate) struct PaneInfo {
    pub(crate) session_name: String,
    pub(crate) session_id: String,
    pub(crate) session_created: Option<u64>,
    pub(crate) window_index: String,
    pub(crate) window_id: String,
    pub(crate) window_created: Option<u64>,
    pub(crate) window_name: String,
    pub(crate) pane_index: String,
    pub(crate) pane_id: String,
    pub(crate) pane_created: Option<u64>,
    pub(crate) pane_pid: u32,
    pub(crate) pane_current_command: String,
    pub(crate) pane_commandline: String,
    pub(crate) pane_current_path: String,
    pub(crate) pane_title: String,
    pub(crate) active_window: bool,
    pub(crate) active_pane: bool,
    pub(crate) busy_duration_secs: Option<u64>,
    pub(crate) gpu_indices: Vec<usize>,
    pub(crate) gpu_memory_by_index: Vec<(usize, u64)>,
}

#[derive(Clone, Debug)]
pub(crate) struct ProcessInfo {
    pub(crate) pid: u32,
    pub(crate) ppid: u32,
    pub(crate) elapsed_secs: u64,
    pub(crate) command: String,
    pub(crate) commandline: String,
}

#[derive(Clone, Debug)]
pub(crate) struct GpuInfo {
    pub(crate) index: usize,
    pub(crate) uuid: String,
    pub(crate) memory_used_mib: u64,
    pub(crate) memory_total_mib: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct GpuProcessInfo {
    pub(crate) gpu_uuid: String,
    pub(crate) pid: u32,
    pub(crate) used_memory_mib: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum GpuBadge {
    Memory {
        digit: char,
        level: u8,
        active: bool,
        placeholder: bool,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct HostTree {
    pub(crate) host: String,
    pub(crate) panes: Vec<PaneInfo>,
    pub(crate) processes: Vec<ProcessInfo>,
    pub(crate) gpus: Vec<GpuInfo>,
    pub(crate) gpu_processes: Vec<GpuProcessInfo>,
    pub(crate) error: Option<String>,
    pub(crate) connecting: bool,
}

#[derive(Clone, Debug)]
pub(crate) enum HostUpdate {
    Panes {
        host: String,
        panes: Vec<PaneInfo>,
        processes: Vec<ProcessInfo>,
        error: Option<String>,
    },
    Gpus {
        host: String,
        gpus: Vec<GpuInfo>,
        gpu_processes: Vec<GpuProcessInfo>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) enum NodeId {
    Host(String),
    Session {
        host: String,
        session: String,
    },
    Window {
        host: String,
        session: String,
        window: String,
    },
    Pane {
        host: String,
        session: String,
        window: String,
        pane: String,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct VisibleRow {
    pub(crate) id: NodeId,
    pub(crate) depth: usize,
    pub(crate) label: String,
    pub(crate) detail: String,
    pub(crate) search_text: String,
    pub(crate) selectable: bool,
    pub(crate) expandable: bool,
    pub(crate) status: RowStatus,
    pub(crate) busy_duration_secs: Option<u64>,
    pub(crate) gpu_badges: Vec<GpuBadge>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RowStatus {
    Normal,
    Unavailable,
}
