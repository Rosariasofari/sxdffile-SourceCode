// This lint is helpful, but right now we have too many instances of it.
// TODO: Remove this once all instances are fixed.
#![allow(clippy::needless_pass_by_ref_mut)]
// This lint is good in theory, but in AVMs we often need to do `let x = args.get(0); let y = args.get(1);` etc.
// It'd make those much less readable and consistent.
#![allow(clippy::get_first)]

#[macro_use]
mod display_object;
pub use display_object::{StageAlign, StageDisplayState, StageScaleMode};

#[macro_use]
extern crate num_derive;

#[macro_use]
mod avm1;
mod avm2;
/// SXDffle：分配归属标签（已下移到 ruffle_common，宿主与引擎共用同一份）。
pub use ruffle_common::alloc_tag;
pub mod sxd_watch;
pub mod loader_stats;
mod avm_rng;
mod binary_data;
pub mod bitmap;
mod character;
pub mod context;
pub mod context_menu;
mod drawing;
mod ecma_conversions;
pub mod events;
pub mod focus_tracker;
pub mod font;
mod frame_lifecycle;
pub mod fte;
mod html;
mod input;
mod library;
pub mod limits;
pub mod loader;
mod local_connection;
mod net_connection;
mod orphan_manager;
pub mod pixel_bender;
mod player;
mod prelude;
pub mod socket;
mod streams;
pub mod string;
mod system_properties;
pub mod tag_utils;
mod tessellation_cache;
pub mod timer;
mod types;
mod vminterface;

pub mod backend;
pub mod compatibility_rules;
pub mod config;
#[cfg(feature = "egui")]
pub mod debug_ui;
pub mod external;
pub mod i18n;
pub mod stub;

pub use context_menu::ContextMenuItem;
pub use events::PlayerEvent;
pub use indexmap;
pub use loader::LoadBehavior;
pub use player::{Player, PlayerBuilder, PlayerMode, PlayerRuntime, StaticCallstack};

/// 内测魔改版：对象布局尺寸（诊断用）。
pub use player::object_layout_report;
pub use ruffle_common::duration::FloatDuration;
pub use ruffle_render::backend::ViewportDimensions;
pub use swf;
pub use swf::Color;
pub use ttf_parser;

/// The newest Flash Player version known to Ruffle.
pub const NEWEST_PLAYER_VERSION: u8 = 51;

/// The default Flash Player version that Ruffle will emulate.
pub const DEFAULT_PLAYER_VERSION: u8 = 32;

/// Path to the raw playerglobal ABC, for use as an `asc.jar` import library.
pub const PLAYERGLOBAL_ABC_PATH: &str = env!("RUFFLE_PLAYERGLOBAL_ABC_PATH");

/// SXDffle：GC 堆字节统计（无锁，可从任意线程调用）。
///
/// 返回 (累计分配字节, 累计释放字节, 当前存活对象数)。
/// 因为底层是 atomic 累加，**不需要持有 Player 锁** —— 这一点很关键：
/// 战报解析期间渲染线程被脚本占住，任何要拿锁的采样都会卡死，
/// 而独立监控线程必须能在这种时刻读到真实数据。
pub fn heap_byte_stats_global() -> (usize, usize, usize) {
    gc_arena::heap_byte_stats()
}

/// SXDffle：SWF 载入体量（个数, 压缩字节, 解压字节）。
///
/// 每个 SWF 解压后是整块常驻的 `Arc<Vec<u8>>`，不受 GC 管理；
/// 这是「GC 堆之外」最大的候选之一。
pub fn swf_load_stats() -> (usize, usize, usize) {
    ruffle_common::tag_utils::swf_load_stats()
}
