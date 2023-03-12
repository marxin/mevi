use std::{borrow::Cow, collections::HashMap, ops::Range};

use futures_util::StreamExt;
use gloo_net::websocket::{futures::WebSocket, Message};
use humansize::{make_format, BINARY};
use itertools::Itertools;
use rangemap::RangeMap;
use serde::{Deserialize, Serialize};
use wasm_bindgen_futures::spawn_local;
use yew::prelude::*;

type MemMap = RangeMap<u64, MemState>;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
enum MemState {
    Resident,
    NotResident,
    Unmapped,
}

struct GroupInfo {
    start: u64,
    size: u64,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Hash)]
#[serde(transparent)]
struct TraceeId(u64);

#[derive(Debug, Deserialize)]
enum MeviEvent {
    Snapshot(Vec<TraceeSnapshot>),
    TraceeEvent(TraceeId, TraceePayload),
}

#[derive(Debug, Clone, Deserialize)]
struct TraceeSnapshot {
    tid: TraceeId,
    cmdline: Vec<String>,
    map: MemMap,
}

#[derive(Debug, Deserialize)]
struct MapGuard {
    #[serde(skip)]
    _inner: Option<()>,
}

#[derive(Debug, Deserialize)]
enum TraceePayload {
    Map {
        range: Range<u64>,
        state: MemState,
        _guard: MapGuard,
    },
    Connected {
        _uffd: u64,
        cmdline: Vec<String>,
    },
    PageIn {
        range: Range<u64>,
    },
    PageOut {
        range: Range<u64>,
    },
    Unmap {
        range: Range<u64>,
    },
    Remap {
        old_range: Range<u64>,
        new_range: Range<u64>,
    },
    Batch {
        batch: MemMap,
    },
}

#[derive(Clone)]
struct TraceeState {
    tid: TraceeId,
    map: MemMap,
    cmdline: Vec<String>,
}

#[function_component(App)]
fn app() -> Html {
    let tracees = use_state(|| -> HashMap<TraceeId, TraceeState> { Default::default() });

    {
        let tracees = tracees.clone();
        use_effect_with_deps(
            move |_| {
                let mut tracees_acc = HashMap::new();

                gloo_console::log!("Connecting to WebSocket...");
                let ws = WebSocket::open("ws://localhost:5001/ws").unwrap();
                gloo_console::log!("Connected to WebSocket");
                let (write, mut read) = ws.split();
                drop(write);

                spawn_local(async move {
                    while let Some(msg) = read.next().await {
                        let msg = msg.unwrap();
                        match msg {
                            Message::Text(t) => {
                                gloo_console::log!(format!("text message: {t}"))
                            }
                            Message::Bytes(b) => {
                                let ev: MeviEvent = bincode::deserialize(&b).unwrap();
                                // gloo_console::log!(format!("{:?}", ev));

                                apply_ev(&mut tracees_acc, ev);
                                tracees.set(tracees_acc.clone());
                            }
                        }
                    }
                    gloo_console::log!("WebSocket Closed")
                })
            },
            (),
        );
    }

    let mut total_virt: u64 = 0;
    let mut total_res: u64 = 0;
    for (range, mem_state) in tracees.values().flat_map(|v| v.map.iter()) {
        if *mem_state != MemState::Unmapped {
            total_virt += range.end - range.start;
        }

        if *mem_state == MemState::Resident {
            total_res += range.end - range.start;
        }
    }

    let formatter = make_format(BINARY);
    html! {
        <>
            <ul style="font-family: monospace;">
                <div>
                    <span class="mem-stats virt"><span class="name">{"Virtual"}</span>{format!("{}", formatter(total_virt))}</span>
                    <span class="mem-stats rss"><span class="name">{"Resident"}</span>{format!("{}", formatter(total_res))}</span>
                </div>
                {{
                    tracees.values().map(|tracee| {
                        html! {
                            <>
                                <div class="process">
                                    <div class="process-info">
                                        {"PID "}{tracee.tid.0}
                                        {" "}
                                        {tracee.cmdline.join(" ")}
                                    </div>
                                    {{
                                        let map = &tracee.map;
                                        let groups = map.iter().group_by(|(range, _)| (range.start >> 40));
                                        let mut group_infos = HashMap::new();
                                        for (key, group) in groups.into_iter() {
                                            let mut group_start: Option<u64> = None;
                                            let mut group_end: Option<u64> = None;
                                            for (range, _state) in group {
                                                if group_start.is_none() {
                                                    group_start = Some(range.start);
                                                }
                                                group_end = Some(range.end);
                                            }
                                            let size = group_end.unwrap() - group_start.unwrap();
                                            group_infos.insert(key, GroupInfo {
                                                start: group_start.unwrap(),
                                                size,
                                            });
                                        }

                                        let largest_group = group_infos.values().map(|info| info.size).max().unwrap_or_default();
                                        let mut max_mb: u64 = 4 * 1024 * 1024;
                                        while max_mb < largest_group {
                                            max_mb *= 2;
                                        }
                                        let max_mb = max_mb as f64;

                                        let groups = map.iter().group_by(|(range, _)| (range.start >> 40));
                                        groups.into_iter().map(
                                            |(key, group)| {
                                                let mut group_markup = vec![];
                                                let mut group_start = None;

                                                for (range, mem_state) in group {
                                                    if group_start.is_none() {
                                                        group_start = Some(range.start);
                                                    }

                                                    let size = range.end - range.start;
                                                    if size < 4 * 4096 {
                                                        continue;
                                                    }

                                                    let style = format!("width: {}%; left: {}%;", size as f64 / max_mb * 100.0, (range.start - group_start.unwrap()) as f64 / max_mb * 100.0);
                                                    group_markup.push(html! {
                                                        <i class={format!("{:?}", mem_state)} style={style}>{
                                                            if matches!(mem_state, MemState::Resident) && size > 4 * 1024 * 1024 {
                                                                Cow::from(formatter(size).to_string())
                                                            } else {
                                                                Cow::from("")
                                                            }
                                                        }</i>
                                                    })
                                                }

                                                html! {
                                                    <div class="group-outer">
                                                        <div class="group-header">
                                                            { format!("{:#x}", group_infos[&key].start) }
                                                        </div>
                                                        <div class="group">
                                                            { group_markup }
                                                        </div>
                                                    </div>
                                                }
                                            }
                                        ).collect::<Vec<_>>()
                                    }}
                                </div>
                            </>
                        }
                    }).collect::<Vec<_>>()
                }}
            </ul>
        </>
    }
}

fn apply_ev(tracees: &mut HashMap<TraceeId, TraceeState>, ev: MeviEvent) {
    let (tid, payload) = match ev {
        MeviEvent::Snapshot(snap_tracees) => {
            for snap_tracee in snap_tracees {
                let tracee = tracees
                    .entry(snap_tracee.tid)
                    .or_insert_with(|| TraceeState {
                        tid: snap_tracee.tid,
                        map: Default::default(),
                        cmdline: Default::default(),
                    });
                tracee.cmdline = snap_tracee.cmdline;
                tracee.map = snap_tracee.map;
            }
            return;
        }
        MeviEvent::TraceeEvent(tid, ev) => (tid, ev),
    };

    let tracee = tracees.entry(tid).or_insert_with(|| TraceeState {
        tid,
        map: Default::default(),
        cmdline: Default::default(),
    });

    match payload {
        TraceePayload::Map { range, state, .. } => {
            tracee.map.insert(range, state);
        }
        TraceePayload::Connected { cmdline, .. } => {
            tracee.cmdline = cmdline;
        }
        TraceePayload::PageIn { range } => {
            tracee.map.insert(range, MemState::Resident);
        }
        TraceePayload::PageOut { range } => {
            tracee.map.insert(range, MemState::NotResident);
        }
        TraceePayload::Unmap { range } => {
            tracee.map.insert(range, MemState::Unmapped);
        }
        TraceePayload::Remap {
            old_range,
            new_range,
        } => {
            tracee.map.insert(old_range, MemState::Unmapped);
            // FIXME: this is wrong but eh.
            tracee.map.insert(new_range, MemState::Resident);
        }
        TraceePayload::Batch { batch } => {
            for (range, mem_state) in batch.into_iter() {
                tracee.map.insert(range, mem_state);
            }
        }
    }
}

fn main() {
    yew::Renderer::<App>::new().render();
}
