use std::{
    cell::RefCell,
    cmp::Ordering,
    collections::{hash_map, HashMap},
    fmt, fs,
    iter::{self, Peekable},
    vec,
};

use data::{ActionKind, ReportConfig, SimData};
use rand::{prelude::Distribution, thread_rng, Rng, SeedableRng};
use rand_pcg::Pcg64;
use serde::{
    de::value::{Error, StrDeserializer},
    Deserialize,
};
use xivc_core::{
    enums::{DamageElement, DamageInstance, Job},
    job::{self, CastError, CdMap, DynJob, JobEvent, State},
    math::{Buffs, EotSnapshot, HitTypeHandle, SpeedStat, XivMath},
    timing::{ActionCd, DurationInfo, ScaleTime},
    world::{
        queue::RadixEventQueue,
        status::{StatusEffect, StatusEvent, StatusEventKind, StatusInstance, StatusSnapshot},
        Action, ActionTargetting, ActorId, ActorRef, CriticalHit, DamageEvent, DamageVariance,
        DirectHit, Event, EventError, EventRng, EventSink, Faction, Positional, WorldRef,
    },
};

mod data;

fn main() {
    let file = fs::read_to_string("./sim.ycf").unwrap();
    let mut deserializer = ycf::de::TopDeserializer::from_str(&file);
    let data: SimData = SimData::deserialize(&mut deserializer).unwrap();
    let end = data.end + data.in_combat;
    let mut sim = Simulation::from_sim_data(data).unwrap();

    while sim.step(end).unwrap() {}

    println!("\n{:#?}", sim);
}

#[derive(Clone, Debug)]
struct Simulation {
    world: WorldState,
    events: RadixEventQueue<SimEvent>,
    rng: SimRngSource,
    actions: HashMap<ActorId, Peekable<vec::IntoIter<ActionKind<Action>>>>,
    report: ReportConfig,
}

#[derive(Clone, Debug)]
enum SimEvent {
    Event(Event),
    StartCast(ActorId, Action),
    CastSnap(ActorId, Action),
    Untargetable(ActorId),
    Targetable(ActorId),
}

#[derive(Debug)]
enum FromSimDataError {
    UnknownAction(Job, String),
}

impl Simulation {
    fn from_sim_data(data: SimData) -> Result<Self, FromSimDataError> {
        let mut events = RadixEventQueue::new();
        let mut actors = Vec::new();
        let mut actions = HashMap::new();
        for player in data.players {
            let id = ActorId(actors.len() as u16);
            let job = player.job;
            let state = ActorState {
                name: player.name,
                damage: 0,
                player: Some(PlayerState {
                    cooldowns: CdMap::default_for(job),
                    gcd: 0,
                    job: DynJob::from_job(job),
                    lock: 0,
                    math: XivMath::new(player.stats, player.weapon, player.player_info),
                    mp: 10000,
                    state: RefCell::new(State::default_for(job)),
                }),
                statuses: HashMap::new(),
                target: None,
                targetable: true,
            };

            events.push(
                player.first_actor_tick,
                SimEvent::Event(Event::ActorTick(id)),
            );
            events.push(player.first_mp_tick, SimEvent::Event(Event::MpTick(id)));

            let mut acs = Vec::new();
            // dumb hack
            for action in player.actions {
                match action {
                    ActionKind::Normal(s) => {
                        acs.push(ActionKind::Normal(Action::Job(
                            job::Action::deserialize_for(job, StrDeserializer::<Error>::new(&*s))
                                .map_err(|_| FromSimDataError::UnknownAction(job, s))?,
                        )));
                    }
                    ActionKind::Delay(d, s) => {
                        acs.push(ActionKind::Delay(
                            d,
                            Action::Job(
                                job::Action::deserialize_for(
                                    job,
                                    StrDeserializer::<Error>::new(&*s),
                                )
                                .map_err(|_| FromSimDataError::UnknownAction(job, s))?,
                            ),
                        ));
                    }
                }
            }

            let mut acs = acs.into_iter().peekable();

            // then push the first one in the list to the event list.
            if let Some(ac) = acs.next() {
                let (t, ac) = match ac {
                    ActionKind::Normal(ac) => (player.first_action, ac),
                    ActionKind::Delay(d, ac) => (player.first_action + d, ac),
                };
                events.push(t, SimEvent::StartCast(id, ac));
            }

            actors.push(state);
            actions.insert(id, acs.into_iter());
        }
        for enemy in data.enemies {
            let id = ActorId(actors.len() as u16);
            let state = ActorState {
                name: enemy.name,
                damage: 0,
                player: None,
                statuses: HashMap::new(),
                target: None,
                targetable: true,
            };
            events.push(
                enemy.first_actor_tick,
                SimEvent::Event(Event::ActorTick(id)),
            );
            actors.push(state);
            for (start, end) in enemy.untarget {
                events.push(start, SimEvent::Untargetable(id));
                events.push(end, SimEvent::Targetable(id));
            }
        }

        Ok(Self {
            events,
            world: WorldState {
                time: 0,
                in_combat: data.in_combat,
                actors,
            },
            rng: SimRngSource {
                rng: Pcg64::from_seed(thread_rng().gen()),
            },
            actions,
            report: data.report,
        })
    }

    fn step(&mut self, end: u32) -> Result<bool, CastError> {
        let Some((time, e)) = self.events.pop() else {
            return Ok(false);
        };

        if time >= end {
            return Ok(false);
        }
        // match &e {
        //     SimEvent::Event(e) => match e {
        //         Event::ActorTick(..) | Event::MpTick(..) => (),
        //         _ => println!("[{:>4}.{:03}]: {:?}", time / 1000, time % 1000, e),
        //     },
        //     SimEvent::StartCast(id, action) => println!(
        //         "[{:>4}.{:03}]: StartCast({:?}, {})",
        //         time / 1000,
        //         time % 1000,
        //         id,
        //         action.name()
        //     ),
        //     SimEvent::CastSnap(id, action) => println!(
        //         "[{:>4}.{:03}]: CastSnap({:?}, {})",
        //         time / 1000,
        //         time % 1000,
        //         id,
        //         action.name()
        //     ),
        //     _ => println!("[{:>4}.{:03}]: {:?}", time / 1000, time % 1000, e),
        // }

        match self.world.time.cmp(&time) {
            Ordering::Equal => (),
            Ordering::Greater => panic!(
                "world time ({}), event queue time ({})",
                self.world.time, time
            ),
            Ordering::Less => self.world.advance(time - self.world.time),
        }
        match e {
            SimEvent::Event(e) => {
                match e {
                    Event::ActorTick(id) => {
                        if let Some(actor) = self.world.actors.get_mut(id.0 as usize) {
                            for effect in actor.statuses.values() {
                                if let Some(snapshot) = &effect.snapshot {
                                    let damage = snapshot.eot_result(
                                        self.rng.random(CriticalHit::new(snapshot.crit_chance)),
                                        self.rng.random(DirectHit::new(snapshot.dhit_chance)),
                                        self.rng.random(DamageVariance::new()),
                                    );

                                    actor.damage += damage as u32;
                                }
                            }
                            self.events
                                .push(time + 3000, SimEvent::Event(Event::ActorTick(id)));
                        }
                    }
                    Event::AddMp(mp, id) => {
                        if let Some(player) = self
                            .world
                            .actors
                            .get_mut(id.0 as usize)
                            .and_then(|a| a.player.as_mut())
                        {
                            player.mp = (player.mp + mp).min(10000);
                        }
                    }
                    Event::AdvCd(cdg, adv, id) => {
                        if let Some(player) = self
                            .world
                            .actors
                            .get_mut(id.0 as usize)
                            .and_then(|a| a.player.as_mut())
                        {
                            if let Some(cd) = player.cooldowns.get_mut(cdg) {
                                cd.advance(adv);
                            }
                        }
                    }
                    Event::Damage(DamageEvent {
                        damage,
                        target,
                        source,
                        action,
                    }) => {
                        if let Some(actor) = self.world.actors.get_mut(target.0 as usize) {
                            actor.damage += damage as u32;

                            if self.report.damage {
                                self.report(
                                    time,
                                    ReportKind::Damage {
                                        source,
                                        target,
                                        action,
                                        damage,
                                    },
                                )
                            }
                        }
                    }
                    Event::Status(StatusEvent {
                        kind,
                        source,
                        status,
                        target,
                    }) => {
                        if let Some(target_actor) = self.world.actors.get_mut(target.0 as usize) {
                            let key = (if status.unique { None } else { Some(source) }, status);
                            let kind = match kind {
                                StatusEventKind::Remove => {
                                    target_actor.statuses.remove(&key);

                                    StatusReportKind::Remove
                                }
                                StatusEventKind::Apply { duration, stacks } => {
                                    target_actor.statuses.insert(
                                        key,
                                        StatusEntry {
                                            instance: StatusInstance {
                                                source,
                                                effect: status,
                                                time: duration,
                                                stack: stacks,
                                            },
                                            snapshot: None,
                                        },
                                    );

                                    StatusReportKind::Apply { duration, stacks }
                                }
                                StatusEventKind::AddStacks { .. } => {
                                    todo!("i don't remember what the semantics of this was supposed to be");
                                }
                                StatusEventKind::ApplyDot {
                                    duration,
                                    snapshot,
                                    stacks,
                                } => {
                                    target_actor.statuses.insert(
                                        key,
                                        StatusEntry {
                                            instance: StatusInstance {
                                                source,
                                                effect: status,
                                                time: duration,
                                                stack: stacks,
                                            },
                                            snapshot: Some(Box::new(snapshot)),
                                        },
                                    );

                                    StatusReportKind::Apply { duration, stacks }
                                }
                                StatusEventKind::RemoveStacks { stacks } => {
                                    if let Some(entry) = target_actor.statuses.get_mut(&key) {
                                        entry.instance.sub_stacks(stacks);
                                        if entry.instance.stack == 0 {
                                            target_actor.statuses.remove(&key);
                                        }
                                    }

                                    StatusReportKind::RemoveStacks { stacks }
                                }
                                StatusEventKind::ApplyOrExtend {
                                    duration,
                                    stacks,
                                    max,
                                } => {
                                    if let Some(entry) = target_actor.statuses.get_mut(&key) {
                                        let from = entry.instance.time;

                                        entry.instance.time =
                                            (entry.instance.time + duration).min(max);

                                        let to = entry.instance.time;

                                        entry.instance.stack = stacks;

                                        StatusReportKind::ExtendDuration {
                                            duration,
                                            stacks,
                                            from,
                                            to,
                                        }
                                    } else {
                                        target_actor.statuses.insert(
                                            key,
                                            StatusEntry {
                                                instance: StatusInstance {
                                                    source,
                                                    effect: status,
                                                    time: duration,
                                                    stack: stacks,
                                                },
                                                snapshot: None,
                                            },
                                        );

                                        StatusReportKind::Apply { duration, stacks }
                                    }
                                }
                                StatusEventKind::ApplyOrAddStacks {
                                    duration,
                                    stacks,
                                    max,
                                } => {
                                    if let Some(entry) = target_actor.statuses.get_mut(&key) {
                                        entry.instance.time = duration;

                                        let from = entry.instance.stack;

                                        entry.instance.add_stacks(stacks, max);

                                        let to = entry.instance.stack;

                                        StatusReportKind::AddStacks {
                                            from,
                                            to,
                                            duration,
                                            stacks,
                                        }
                                    } else {
                                        target_actor.statuses.insert(
                                            key,
                                            StatusEntry {
                                                instance: StatusInstance {
                                                    source,
                                                    effect: status,
                                                    time: duration,
                                                    stack: stacks,
                                                },
                                                snapshot: None,
                                            },
                                        );

                                        StatusReportKind::Apply { duration, stacks }
                                    }
                                }
                            };
                            if self.report.status {
                                self.report(
                                    time,
                                    ReportKind::Status {
                                        status,
                                        source,
                                        target,
                                        kind,
                                    },
                                )
                            }
                        }
                    }
                    Event::MpTick(id) => {
                        if let Some(player) = self
                            .world
                            .actors
                            .get_mut(id.0 as usize)
                            .and_then(|a| a.player.as_mut())
                        {
                            let from = player.mp;

                            let tick = player.math.mp_regen() as u16;
                            player.mp = (player.mp + tick).min(10000);

                            let to = player.mp;

                            self.events
                                .push(time + 3000, SimEvent::Event(Event::ActorTick(id)));

                            if self.report.mp_tick {
                                self.report(
                                    time,
                                    ReportKind::MpTick {
                                        actor: id,
                                        from,
                                        to,
                                        tick,
                                    },
                                )
                            }
                        }
                    }
                    Event::Job(ref job_event, actor) => {
                        if self.report.job_event {
                            self.report(
                                time,
                                ReportKind::JobEvent {
                                    event: job_event.clone(),
                                    actor,
                                },
                            )
                        }
                    } // just a job event, that gets handled down below.
                }

                for (actor, player) in self
                    .world
                    .iter_actors()
                    .filter_map(|actor| actor.actor.player.as_ref().map(|v| (actor, v)))
                {
                    let mut state = { player.state.borrow().clone() };

                    let mut event_sink =
                        SimEventSink::new(actor, &mut self.rng, &mut self.events, time);

                    player
                        .job
                        .event(&mut state, &&self.world, &e, &mut event_sink);

                    *player.state.borrow_mut() = state;
                }
            }
            SimEvent::Targetable(id) => {
                if let Some(actor) = self.world.actors.get_mut(id.0 as usize) {
                    actor.targetable = true;

                    if self.report.target {
                        self.report(
                            time,
                            ReportKind::Target {
                                actor: id,
                                can_target: true,
                            },
                        )
                    }
                }
            }
            SimEvent::Untargetable(id) => {
                if let Some(actor) = self.world.actors.get_mut(id.0 as usize) {
                    actor.targetable = false;

                    if self.report.target {
                        self.report(
                            time,
                            ReportKind::Target {
                                actor: id,
                                can_target: false,
                            },
                        )
                    }
                }
            }
            SimEvent::StartCast(id, action) => {
                if let Some((actor, player)) = self
                    .world
                    .actors
                    .get(id.0 as usize)
                    .and_then(|actor| actor.player.as_ref().map(|v| (actor, v)))
                {
                    match action {
                        Action::Job(action) => {
                            let state = player.state.borrow();

                            let mut event_sink = SimEventSink::new(
                                ActorHandle {
                                    actor,
                                    id,
                                    world: &self.world,
                                },
                                &mut self.rng,
                                &mut self.events,
                                time,
                            );

                            let info = player.job.check_cast(
                                action,
                                &*state,
                                &&self.world,
                                &mut event_sink,
                            );

                            drop(state);

                            // dear god.

                            let player = self
                                .world
                                .actors
                                .get_mut(id.0 as usize)
                                .unwrap()
                                .player
                                .as_mut()
                                .unwrap();

                            let actions = self.actions.get_mut(&id).unwrap();

                            let gcd_left = info.gcd.max(player.gcd);
                            let lock_left = info.lock;

                            if let Some(next) = actions.next() {
                                let (d, next) = match next {
                                    ActionKind::Normal(ac) => (0, ac),
                                    ActionKind::Delay(d, ac) => (d, ac),
                                };
                                if next.gcd() {
                                    self.events.push(
                                        d + time + (gcd_left.max(lock_left) as u32),
                                        SimEvent::StartCast(id, next),
                                    );
                                } else {
                                    self.events.push(
                                        d + time + lock_left as u32,
                                        SimEvent::StartCast(id, next),
                                    );
                                }
                            } else {
                                self.actions.remove(&id);
                            }

                            player.gcd = gcd_left;
                            player.lock = lock_left;

                            player.mp = match player.mp.checked_sub(info.mp) {
                                Some(v) => v,
                                None => panic!("Not enough MP at {} for {:?}", time, action),
                            };

                            if let Some((cdg, cd, charges)) = info.cd {
                                if let Some(cd_state) = player.cooldowns.get_mut(cdg) {
                                    if !cd_state.available(cd, charges) {
                                        panic!(
                                            "Intersecting cooldown at {} for {:?}. Minimum time until ok is {}.",
                                            cd,
                                            action,
                                            cd_state.cd_until(cd, charges)
                                        );
                                    }
                                    cd_state.apply(cd, charges);
                                }
                            }

                            if let Some((cdg, cd, charges)) = info.alt_cd {
                                if let Some(cd_state) = player.cooldowns.get_mut(cdg) {
                                    if !cd_state.available(cd, charges) {
                                        panic!(
                                            "Intersecting cooldown at {} for {:?}. Minimum time until ok is {}.",
                                            cd,
                                            action,
                                            cd_state.cd_until(cd, charges)
                                        );
                                    }
                                    cd_state.apply(cd, charges);
                                }
                            }

                            self.events.push(
                                time + info.snap as u32,
                                SimEvent::CastSnap(id, Action::Job(action)),
                            );
                        }
                    }

                    if self.report.cast_start {
                        self.report(time, ReportKind::CastStart { source: id, action })
                    }
                }
            }
            SimEvent::CastSnap(id, action) => {
                if let Some((actor, player)) = self
                    .world
                    .actors
                    .get(id.0 as usize)
                    .and_then(|actor| actor.player.as_ref().map(|v| (actor, v)))
                {
                    match action {
                        Action::Job(action) => {
                            let mut state = { player.state.borrow().clone() };

                            let mut event_sink = SimEventSink::new(
                                ActorHandle {
                                    actor,
                                    id,
                                    world: &self.world,
                                },
                                &mut self.rng,
                                &mut self.events,
                                time,
                            );

                            player
                                .job
                                .cast_snap(action, &mut state, &&self.world, &mut event_sink);

                            *player.state.borrow_mut() = state;
                        }
                    }

                    if self.report.cast_snap {
                        self.report(time, ReportKind::CastSnap { source: id, action })
                    }
                }
            }
        }

        Ok(true)
    }

    fn report(&self, time: u32, kind: ReportKind) {
        print!("{:>4}.{:03}: ", time / 1000, time % 1000);
        println!(
            "{}",
            ReportData {
                kind,
                world: &self.world
            }
        );
    }
}

struct ReportData<'w> {
    kind: ReportKind,
    world: &'w WorldState,
}

impl<'w> fmt::Display for ReportData<'w> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use ReportKind::*;
        let name = |id: ActorId| {
            self.world
                .actors
                .get(id.0 as usize)
                .map(|v| v.name.as_str())
                .unwrap_or_default()
        };
        match &self.kind {
            MpTick {
                actor,
                from,
                to,
                tick,
            } => f
                .debug_struct("MpTick")
                .field("actor", &name(*actor))
                .field("from", from)
                .field("to", to)
                .field("tick", tick)
                .finish(),
            Damage {
                source,
                target,
                action,
                damage,
            } => f
                .debug_struct("Damage")
                .field("source", &name(*source))
                .field("target", &name(*target))
                .field("action", &action.name())
                .field("damage", damage)
                .finish(),
            Status {
                status,
                source,
                target,
                kind,
            } => f
                .debug_struct("Status")
                .field("status", &status.name)
                .field("source", &name(*source))
                .field("target", &name(*target))
                .field("kind", kind)
                .finish(),
            CastStart { source, action } => f
                .debug_struct("CastStart")
                .field("source", &name(*source))
                .field("action", &action.name())
                .finish(),
            CastSnap { source, action } => f
                .debug_struct("CastSnap")
                .field("source", &name(*source))
                .field("action", &action.name())
                .finish(),
            JobEvent { event, actor } => f
                .debug_struct("JobEvent")
                .field("event", event)
                .field("actor", &name(*actor))
                .finish(),
            Target { actor, can_target } => f
                .debug_struct("Target")
                .field("actor", &name(*actor))
                .field("can_target", can_target)
                .finish(),
        }
    }
}

enum ReportKind {
    MpTick {
        actor: ActorId,
        from: u16,
        to: u16,
        tick: u16,
    },
    Damage {
        source: ActorId,
        target: ActorId,
        action: Action,
        damage: u64,
    },
    Status {
        status: StatusEffect,
        source: ActorId,
        target: ActorId,
        kind: StatusReportKind,
    },
    CastStart {
        source: ActorId,
        action: Action,
    },
    CastSnap {
        source: ActorId,
        action: Action,
    },
    JobEvent {
        event: JobEvent,
        actor: ActorId,
    },
    Target {
        actor: ActorId,
        can_target: bool,
    },
}

#[derive(Debug)]
#[allow(unused)]
enum StatusReportKind {
    Apply {
        duration: u32,
        stacks: u8,
    },
    Remove,
    NaturalRemove,
    RemoveStacks {
        stacks: u8,
    },
    ExtendDuration {
        from: u32,
        to: u32,
        duration: u32,
        stacks: u8,
    },
    AddStacks {
        from: u8,
        to: u8,
        duration: u32,
        stacks: u8,
    },
}

#[derive(Clone, Debug)]
struct WorldState {
    time: u32,
    in_combat: u32,
    actors: Vec<ActorState>,
}

impl WorldState {
    fn advance(&mut self, time: u32) {
        for actor in self.actors.iter_mut() {
            if let Some(player) = &mut actor.player {
                player.cooldowns.iter_mut().for_each(|x| x.advance(time));
                player.gcd = (player.gcd as u32).saturating_sub(time) as u16;
                player.lock = (player.lock as u32).saturating_sub(time) as u16;
                player.state.borrow_mut().advance(time);
            }
            let mut to_remove = Vec::new();
            for (key, status) in actor.statuses.iter_mut() {
                status.instance.advance(time);
                if status.instance.time == 0 {
                    to_remove.push(*key);
                }
            }
            for x in &to_remove {
                actor.statuses.remove(&x);
            }
        }
        self.time += time;
    }
}

impl WorldState {
    fn iter_actors(&self) -> impl Iterator<Item = ActorHandle<'_>> {
        self.actors
            .iter()
            .enumerate()
            .map(|(id, actor)| ActorHandle {
                world: self,
                id: ActorId(id as u16),
                actor,
            })
    }
}

#[derive(Clone, Debug)]
struct ActorState {
    // The name of the actor.
    name: String,
    // true if the actor can be targeted.
    targetable: bool,
    // the amount of total damage taken.
    damage: u32,
    // the statuses an actor has.
    statuses: HashMap<(Option<ActorId>, StatusEffect), StatusEntry>,
    // if this actor is a player, the corresponding state.
    player: Option<PlayerState>,

    target: Option<ActorId>,
}

#[derive(Clone, Debug)]
struct StatusEntry {
    // if only one instance of the effect can exist on the target at the same time,
    // this will be none, otherwise it will be the source ActorId.
    instance: StatusInstance,
    snapshot: Option<Box<EotSnapshot>>,
}

#[derive(Clone, Debug)]
struct PlayerState {
    job: DynJob,
    gcd: u16,
    lock: u16,
    mp: u16,
    cooldowns: CdMap<ActionCd>,
    state: RefCell<State>,
    math: XivMath,
}

impl<'w> WorldRef<'w> for &'w WorldState {
    type Actor = ActorHandle<'w>;

    type DurationInfo = ActorDurInfo<'w>;

    fn actor(&self, id: ActorId) -> Option<Self::Actor> {
        self.actors.get(id.0 as usize).map(|state| ActorHandle {
            actor: state,
            id,
            world: self,
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct ActorHandle<'w> {
    actor: &'w ActorState,
    id: ActorId,
    world: &'w WorldState,
}

impl<'w> ActorHandle<'w> {
    fn status_iter(&self) -> StatusIter<'w> {
        StatusIter {
            inner: Some(self.actor.statuses.values()),
        }
    }
}

impl<'w> ActorRef<'w> for ActorHandle<'w> {
    type World = &'w WorldState;

    fn id(&self) -> ActorId {
        self.id
    }

    fn world(&self) -> Self::World {
        self.world
    }

    fn attack_damage<R: EventRng>(
        &self,
        damage: DamageInstance,
        target: ActorId,
        rng: &mut R,
    ) -> u64 {
        let DamageInstance {
            potency,
            dmg_el,
            dmg_ty,
            force_crit,
            force_dhit,
        } = damage;
        let state = self
            .actor
            .player
            .as_ref()
            .map(|v| (v.job, v.state.borrow()));
        let buffs = StatusSnapshot::new(
            self.status_iter(),
            self.world
                .actor(target)
                .map(|target| target.status_iter())
                .unwrap_or_default(),
            state.as_ref().and_then(|(job, state)| job.effect(&*state)),
        );
        if let Some(player) = &self.actor.player {
            let ch = match force_crit {
                true => HitTypeHandle::Force,
                false => {
                    let chance = buffs.crit_chance(player.math.crit_chance());
                    rng.random(CriticalHit::new(chance as u16))
                }
            };
            let dh = match force_dhit {
                true => HitTypeHandle::Force,
                false => {
                    let chance = buffs.dhit_chance(player.math.dhit_chance());
                    rng.random(DirectHit::new(chance as u16))
                }
            };
            player.math.action_damage(
                potency,
                dmg_ty,
                dmg_el,
                player.math.job_attack_stat(),
                ch,
                dh,
                rng.random(DamageVariance::new()),
                &buffs,
            )
        } else {
            buffs.damage(potency, dmg_ty, dmg_el)
        }
    }

    fn dot_damage_snapshot(
        &self,
        damage: DamageInstance,
        stat: SpeedStat,
        target: ActorId,
    ) -> EotSnapshot {
        let DamageInstance {
            potency,
            dmg_el,
            dmg_ty,
            ..
        } = damage;
        let state = self
            .actor
            .player
            .as_ref()
            .map(|v| (v.job, v.state.borrow()));
        let buffs = StatusSnapshot::new(
            self.status_iter(),
            self.world
                .actor(target)
                .map(|target| target.status_iter())
                .unwrap_or_default(),
            state.as_ref().and_then(|(job, state)| job.effect(&*state)),
        );
        if let Some(player) = &self.actor.player {
            player.math.dot_damage_snapshot(
                potency,
                dmg_ty,
                dmg_el,
                player.math.job_attack_stat(),
                stat,
                &buffs,
            )
        } else {
            EotSnapshot {
                base: buffs.damage(potency, dmg_ty, dmg_el),
                crit_chance: 0,
                crit_damage: 0,
                dhit_chance: 0,
            }
        }
    }

    fn auto_damage<R: EventRng>(&self, target: ActorId, rng: &mut R) -> u64 {
        let Some(player) = self.actor.player.as_ref() else {
            return 0;
        };
        let state = player.state.borrow();
        let potency = player.job.job().aa_potency();
        let dmg_ty = player.job.job().aa_type();
        let buffs = StatusSnapshot::new(
            self.status_iter(),
            self.world
                .actor(target)
                .map(|target| target.status_iter())
                .unwrap_or_default(),
            player.job.effect(&*state),
        );
        let ch = {
            let chance = buffs.crit_chance(player.math.crit_chance());
            rng.random(CriticalHit::new(chance as u16))
        };
        let dh = {
            let chance = buffs.dhit_chance(player.math.dhit_chance());
            rng.random(DirectHit::new(chance as u16))
        };
        player.math.aa_damage(
            potency as u64,
            dmg_ty,
            DamageElement::None,
            ch,
            dh,
            rng.random(DamageVariance::new()),
            &buffs,
        )
    }

    fn statuses(&self) -> impl Iterator<Item = StatusInstance> {
        self.actor.statuses.values().map(|status| status.instance)
    }

    fn target(&self) -> Option<Self> {
        self.actor.target.and_then(|id| self.world.actor(id))
    }

    fn actors_for_action(
        &self,
        faction: Option<Faction>,
        _: ActionTargetting,
    ) -> impl Iterator<Item = Self> {
        self.world.iter_actors().filter(move |handle| {
            handle.actor.targetable
                && match faction {
                    Some(faction) => match faction {
                        Faction::Enemy => handle.faction() == Faction::Enemy,
                        Faction::Friendly => handle.faction() != Faction::Enemy,
                        Faction::Party => handle.faction() == Faction::Party,
                    },
                    None => true,
                }
        })
    }

    fn within_range(&self, _: ActorId, _: ActionTargetting) -> bool {
        true
    }

    fn mp(&self) -> u16 {
        self.actor.player.as_ref().map(|v| v.mp).unwrap_or_default()
    }

    fn faction(&self) -> Faction {
        if self.actor.player.is_some() {
            Faction::Party
        } else {
            Faction::Enemy
        }
    }

    fn check_positional(&self, _: Positional, _: ActorId) -> bool {
        true
    }

    fn in_combat(&self) -> bool {
        self.world.time >= self.world.in_combat
    }

    fn duration_info(&self) -> <Self::World as WorldRef<'w>>::DurationInfo {
        ActorDurInfo { actor: *self }
    }
}

struct ActorDurInfo<'w> {
    actor: ActorHandle<'w>,
}

impl<'w> DurationInfo for ActorDurInfo<'w> {
    fn extra_ani_lock(&self) -> u16 {
        self.actor
            .actor
            .player
            .as_ref()
            .map(|v| v.math.ex_lock)
            .unwrap_or_default()
    }

    fn scale(&self, duration: ScaleTime) -> u32 {
        (if let Some(player) = self.actor.actor.player.as_ref() {
            if duration.haste() {
                let state = player.state.borrow();
                let buffs = StatusSnapshot {
                    job: player.job.effect(&*state),
                    source: self
                        .actor
                        .actor
                        .statuses
                        .values()
                        .map(|status| status.instance),
                    // target statuses don't play a part here.
                    target: iter::empty(),
                };
                player
                    .math
                    .action_cast_length(duration.duration() as u64, duration.stat(), &buffs)
            } else {
                player.math.action_cast_length(
                    duration.duration() as u64,
                    duration.stat(),
                    &StatusSnapshot::empty(),
                )
            }
        } else {
            duration.duration() as u64
        }) as u32
    }
}

#[derive(Clone, Debug, Default)]
struct StatusIter<'w> {
    inner: Option<hash_map::Values<'w, (Option<ActorId>, StatusEffect), StatusEntry>>,
}

impl<'w> Iterator for StatusIter<'w> {
    type Item = StatusInstance;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.as_mut()?.next().map(|v| v.instance)
    }
}

struct SimEventSink<'w> {
    source: ActorHandle<'w>,
    rng: &'w mut SimRngSource,
    event: &'w mut RadixEventQueue<SimEvent>,
    base: u32,
}

impl<'w> SimEventSink<'w> {
    pub fn new(
        source: ActorHandle<'w>,
        rng: &'w mut SimRngSource,
        event: &'w mut RadixEventQueue<SimEvent>,
        base: u32,
    ) -> Self {
        Self {
            source,
            rng,
            event,
            base,
        }
    }
}

impl<'w> EventSink<'w, &'w WorldState> for SimEventSink<'w> {
    type Rng = SimRngSource;

    fn source(&self) -> ActorHandle<'w> {
        self.source
    }

    fn error(&mut self, error: EventError) {
        panic!("{:?}", error)
    }

    fn event(&mut self, event: Event, delay: u32) {
        let time = self.base + delay;
        self.event.push(time, SimEvent::Event(event));
    }

    fn rng(&mut self) -> &mut Self::Rng {
        &mut self.rng
    }
}

#[derive(Clone, Debug)]
struct SimRngSource {
    rng: Pcg64,
}

impl EventRng for SimRngSource {
    fn random<D, T>(&mut self, distr: D) -> T
    where
        D: Distribution<T> + 'static,
        T: 'static,
    {
        self.rng.sample(distr)
    }
}

struct FmtStacks(u8);

impl fmt::Display for FmtStacks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 > 1 {
            write!(f, "({})", self.0)
        } else {
            write!(f, "")
        }
    }
}
