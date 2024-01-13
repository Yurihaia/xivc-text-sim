use serde::{Deserialize, Serialize};
use xivc_core::{enums::Job, math::{PlayerStats, WeaponInfo, PlayerInfo}};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SimData {
    pub players: Vec<PlayerData>,
    pub enemies: Vec<EnemyData>,
    pub in_combat: u32,
    pub end: u32,
    #[serde(default)]
    pub report: ReportConfig,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ReportConfig {
    pub mp_tick: bool,
    pub damage: bool,
    pub status: bool,
    pub cast_start: bool,
    pub cast_snap: bool,
    pub job_event: bool,
    pub target: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlayerData {
    pub name: String,
    pub job: Job,
    #[serde(with = "StatData")]
    #[serde(default = "StatData::new")]
    pub stats: PlayerStats,
    pub weapon: WeaponInfo,
    pub player_info: PlayerInfo,
    
    #[serde(default)]
    pub first_actor_tick: u32,
    #[serde(default)]
    pub first_mp_tick: u32,
    #[serde(default)]
    pub first_action: u32,
    #[serde(default)]
    pub actions: Vec<ActionKind<String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ActionKind<T> {
    Normal(T),
    Delay(u32, T),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnemyData {
    pub name: String,
    #[serde(default)]
    pub first_actor_tick: u32,
    // the periods in time when this enemy is untargetable
    #[serde(default)]
    pub untarget: Vec<(u32, u32)>,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "PlayerStats")]
#[serde(default = "StatData::new")]
struct StatData {
    pub str: u16,
    pub vit: u16,
    pub dex: u16,
    pub int: u16,
    pub mnd: u16,
    pub det: u16,
    pub crt: u16,
    pub dh: u16,
    pub sks: u16,
    pub sps: u16,
    pub ten: u16,
    pub pie: u16,
}

impl StatData {
    fn new() -> PlayerStats {
        PlayerStats::default(90)
    }
}
