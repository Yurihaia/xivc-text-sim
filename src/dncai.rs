#![allow(overlapping_range_endpoints)]

use xivc_core::{
    job::{
        dnc::{
            DncAction, DncState, Step, StepGauge, DANCE_OF_THE_DAWN_READY, FAN_DANCE_3,
            FAN_DANCE_4, FINISHING_MOVE_READY, FLOURISH_FINISH, FLOURISH_FLOW, FLOURISH_SYMM,
            LAST_DANCE_READY, SILKEN_FLOW, SILKEN_SYMM, STARFALL,
        },
        JobAction, State,
    },
    world::{
        queue::RadixEventQueue,
        status::{StatusEffect, StatusEvent, StatusEventKind},
        ActorId, ActorRef, Event,
    },
};

use crate::{
    jobai::{Controller, JobAiCoro, ResumeCtx},
    ActorHandle, JobAi, PlayerState, SimEvent,
};

pub struct DncAi {
    _id: ActorId,
    coro: JobAiCoro,
}

impl DncAi {
    pub fn new(id: ActorId) -> Self {
        Self {
            _id: id,
            coro: JobAiCoro::new(coroutine),
        }
    }
}

fn state(c: &Controller<'_>) -> DncState {
    let lock = c.ctx().actor.actor.player.as_ref().unwrap().state.borrow();
    let State::Dnc(state) = lock.clone() else {
        panic!()
    };
    state
}

fn player<'w>(c: &'w Controller<'w>) -> &'w PlayerState {
    c.ctx().actor.actor.player.as_ref().unwrap()
}

fn cooldown(c: &Controller, action: DncAction) -> u32 {
    let (g, cd, ch) = action.cd_info().unwrap();

    player(c).cooldowns.get(g.into()).unwrap().cd_until(cd, ch)
}

async fn cast(c: &mut Controller<'_>, action: DncAction) {
    // analyze warnings.
    use DncAction::*;
    c.wait_lock().await;
    let state = state(c);
    match action {
        FanDance | FanDance2 | Flourish if has_status(c, FAN_DANCE_3) => {
            eprintln!("[warn] fan dance 3 potentially overwritten.");
        }
        ReverseCascade | Fountainfall => {
            if state.feathers == 4 {
                eprintln!("[warn] feather potentially overwritten.");
            }
            if state.esprit > 80 {
                eprintln!("[warn] esprit potentially overcapped.");
            }
        }
        StandardStep | FinishingMove if has_status(c, LAST_DANCE_READY) => {
            eprintln!("[warn] last dance overwritten.");
        }
        Cascade => {
            if state.esprit > 85 {
                eprintln!("[warn] esprit potentially overcapped.");
            }
            if has_status(c, SILKEN_SYMM) {
                eprintln!("[warn] silken symmetry potentially overwritten.");
            }
            if state.combos.check_main_for(Fountain) {
                eprintln!("[warn] fountain combo overwritten.");
            }
        }
        Fountain => {
            if state.esprit > 85 {
                eprintln!("[warn] esprit potentially overcapped.");
            }
            if has_status(c, SILKEN_FLOW) {
                eprintln!("[warn] silken flow potentially overwritten.");
            }
        }
        Tillana => {
            eprintln!("[info] tillana used at {} esprit", state.esprit.value());
        }
        _ => (),
    }

    c.cast(action.into()).await;
}

async fn correct_step(c: &mut Controller<'_>) {
    let state = state(c);
    let next = match state.step {
        StepGauge::Std { steps, completed } => {
            if let Some(v) = steps.get(completed as usize) {
                *v
            } else {
                cast(c, DncAction::StandardFinish).await;
                return;
            }
        }
        StepGauge::Tech { steps, completed } => {
            if let Some(v) = steps.get(completed as usize) {
                *v
            } else {
                cast(c, DncAction::TechnicalFinish).await;
                return;
            }
        }
        StepGauge::None => return,
    };
    let step = match next {
        Step::Jete => DncAction::Jete,
        Step::Emboite => DncAction::Emboite,
        Step::Entrechat => DncAction::Entrechat,
        Step::Pirouette => DncAction::Pirouette,
    };

    cast(c, step).await;
}

async fn saber_dance(c: &mut Controller<'_>) {
    if c.ctx().actor.has_own_status(DANCE_OF_THE_DAWN_READY) {
        cast(c, DncAction::DanceOfTheDawn).await;
    } else {
        cast(c, DncAction::SaberDance).await;
    }
}

async fn next_feather(c: &mut Controller<'_>) -> bool {
    c.wait_lock().await;
    if has_status(c, FAN_DANCE_3) {
        cast(c, DncAction::FanDance3).await;
        true
    } else if state(c).feathers > 0 {
        cast(c, DncAction::FanDance).await;
        true
    } else if has_status(c, FAN_DANCE_4) {
        cast(c, DncAction::FanDance4).await;
        true
    } else {
        false
    }
}

async fn burst_prio_combo(c: &mut Controller<'_>) {
    // TODO: Get buff duration priorities to work?
    // may not be nescessary when going in tech correctly.
    c.wait_gcd().await;
    if has_status(c, FLOURISH_FLOW) || has_status(c, SILKEN_FLOW) {
        cast(c, DncAction::Fountainfall).await;
    } else if has_status(c, FLOURISH_SYMM) || has_status(c, SILKEN_SYMM) {
        cast(c, DncAction::ReverseCascade).await;
    } else if state(c).combos.check_main_for(DncAction::Fountain) {
        cast(c, DncAction::Fountain).await;
    } else {
        cast(c, DncAction::Cascade).await;
    }
}

// starts at the first weave slot before the next gcd.
// this is so feathers can be pooled correctly.
async fn filler_prio(c: &mut Controller<'_>) {
    use DncAction::*;
    c.wait_lock().await;
    let (action, genfeather) = if has_status(c, LAST_DANCE_READY) {
        (LastDance, false)
    } else if state(c).esprit >= 70 {
        (SaberDance, false)
    } else if has_status(c, FLOURISH_FLOW) || has_status(c, SILKEN_FLOW) {
        (Fountainfall, true)
    } else if has_status(c, FLOURISH_SYMM) || has_status(c, SILKEN_SYMM) {
        (ReverseCascade, true)
    } else if state(c).combos.check_main_for(Fountain) {
        (Fountain, false)
    } else {
        (Cascade, false)
    };

    let flourish = cooldown(c, Flourish);

    let feather_limit = if flourish < cooldown(c, Devilment) {
        3
    } else {
        4
    };

    if flourish <= (player(c).gcd as u32 - 650) {
        if has_status(c, FAN_DANCE_3) {
            cast(c, FanDance3).await;
        }
        cast(c, Flourish).await;
    } else if genfeather && state(c).feathers == feather_limit {
        if has_status(c, FAN_DANCE_3) {
            cast(c, FanDance3).await;
            cast(c, FanDance).await;
        } else {
            cast(c, FanDance).await;
            c.wait_lock().await;
            if player(c).gcd >= 650 {
                if has_status(c, FAN_DANCE_3) {
                    cast(c, FanDance3).await;
                } else if has_status(c, FAN_DANCE_4) {
                    cast(c, FanDance4).await;
                }
            }
        }
    } else {
        if has_status(c, FAN_DANCE_4) {
            cast(c, FanDance4).await;
        }
    }

    cast(c, action).await;
}

async fn filler_standard(c: &mut Controller<'_>) {
    if has_status(c, FAN_DANCE_4) {
        cast(c, DncAction::FanDance4).await;
    }
    if has_status(c, FAN_DANCE_3) {
        cast(c, DncAction::FanDance3).await;
    }
    if c.ctx().actor.has_own_status(FINISHING_MOVE_READY) {
        cast(c, DncAction::FinishingMove).await;
    } else {
        cast(c, DncAction::StandardStep).await;
        correct_step(c).await;
        correct_step(c).await;
        cast(c, DncAction::StandardFinish).await;
    }
}

async fn pretech_prio(c: &mut Controller<'_>) {
    use DncAction::*;
    c.wait_lock().await;

    let (action, genfeather) = if state(c).esprit >= 50 {
        (SaberDance, false)
    } else if has_status(c, FLOURISH_FLOW) || has_status(c, SILKEN_FLOW) {
        (Fountainfall, true)
    } else if has_status(c, FLOURISH_SYMM) || has_status(c, SILKEN_SYMM) {
        (ReverseCascade, true)
    } else if state(c).combos.check_main_for(Fountain) {
        (Fountain, false)
    } else {
        (Cascade, false)
    };

    if genfeather && state(c).feathers == 4 {
        if has_status(c, FAN_DANCE_3) {
            cast(c, FanDance3).await;
        }
        cast(c, FanDance).await;
    }

    cast(c, action).await;
}

fn has_status(c: &Controller, status: StatusEffect) -> bool {
    c.ctx().actor.has_own_status(status)
}

fn burst_state(c: &Controller) -> (u8, bool, bool, bool) {
    (
        state(c).esprit.value(),
        has_status(c, LAST_DANCE_READY),
        has_status(c, STARFALL),
        has_status(c, FLOURISH_FINISH),
    )
}

async fn feather_weaves(c: &mut Controller<'_>) {
    if next_feather(c).await {
        next_feather(c).await;
    }
}

async fn burst(c: &mut Controller<'_>) {
    use DncAction::*;
    cast(c, DncAction::TechnicalStep).await;
    correct_step(c).await;
    correct_step(c).await;
    correct_step(c).await;
    correct_step(c).await;
    cast(c, DncAction::TechnicalFinish).await;
    cast(c, Devilment).await;
    
    eprintln!("pool status:");
    eprintln!("    {} feathers", state(c).feathers.value());
    eprintln!("    {} fan dance 3", if has_status(c, FAN_DANCE_3) { "yes" } else { "no" });
    eprintln!("    {} esprit", state(c).esprit.value());
    eprintln!("    {} last dance", if has_status(c, LAST_DANCE_READY) { "yes" } else { "no" });

    // first gcd
    c.wait_gcd().await;
    match burst_state(c) {
        (50.., _, _, _) => saber_dance(c).await,
        (0..=20, _, _, true) => cast(c, Tillana).await,
        (0..=30, false, _, true) => cast(c, Tillana).await,
        (_, true, _, _) => cast(c, LastDance).await,
        (_, _, true, _) => cast(c, StarfallDance).await,
        _ => burst_prio_combo(c).await,
    }

    if has_status(c, FAN_DANCE_3) {
        cast(c, FanDance3).await;
        cast(c, Flourish).await;
    } else {
        cast(c, Flourish).await;
        if player(c).gcd >= 650 {
            next_feather(c).await;
        }
    }

    // second gcd
    c.wait_gcd().await;
    match burst_state(c) {
        (50.., _, _, _) => saber_dance(c).await,
        (0..=20, false, _, true) => cast(c, Tillana).await,
        (_, true, _, _) => cast(c, LastDance).await,
        (_, _, true, _) => cast(c, StarfallDance).await,
        _ => burst_prio_combo(c).await,
    }
    feather_weaves(c).await;

    // third gcd

    c.wait_gcd().await;
    match burst_state(c) {
        (_, true, _, _) => cast(c, LastDance).await,
        (50.., _, _, _) => saber_dance(c).await,
        (_, _, true, _) => cast(c, StarfallDance).await,
        _ => burst_prio_combo(c).await,
    }
    feather_weaves(c).await;

    // fourth gcd - finishing move

    cast(c, FinishingMove).await;
    feather_weaves(c).await;

    // fifth gcd

    c.wait_gcd().await;
    match burst_state(c) {
        (50.., _, _, _) => saber_dance(c).await,
        (0..=30, _, _, true) => cast(c, Tillana).await,
        (_, _, true, _) => cast(c, StarfallDance).await,
        (_, true, _, _) => cast(c, LastDance).await,
        _ => burst_prio_combo(c).await,
    }
    feather_weaves(c).await;

    // sixth gcd

    c.wait_gcd().await;
    match burst_state(c) {
        (50.., _, _, _) => saber_dance(c).await,
        (0..=30, _, _, true) => cast(c, Tillana).await,
        (_, _, true, _) => cast(c, StarfallDance).await,
        (_, true, _, _) => cast(c, LastDance).await,
        _ => burst_prio_combo(c).await,
    }
    feather_weaves(c).await;

    // seventh gcd

    c.wait_gcd().await;
    match burst_state(c) {
        (80.., _, _, _) => saber_dance(c).await,
        (_, _, true, _) => cast(c, StarfallDance).await,
        (50.., _, _, _) => saber_dance(c).await,
        (0..=30, _, _, true) => cast(c, Tillana).await,
        (_, true, _, _) => cast(c, LastDance).await,
        _ => burst_prio_combo(c).await,
    }
    feather_weaves(c).await;

    // eigth gcd

    c.wait_gcd().await;
    match burst_state(c) {
        (_, _, true, _) => cast(c, StarfallDance).await,
        (0..=50, _, _, true) => cast(c, Tillana).await,
        (50.., _, _, _) => saber_dance(c).await,
        (_, true, _, _) => cast(c, LastDance).await,
        _ => burst_prio_combo(c).await,
    }
    // the last gcd can only fit a single feather
    next_feather(c).await;
}

async fn coroutine(mut c: Controller<'_>) {
    let c = &mut c;
    use DncAction::*;
    // Opener
    cast(c, StandardStep).await;
    correct_step(c).await;
    correct_step(c).await;
    c.wait(12000).await;
    cast(c, StandardFinish).await;
    // Start of loop
    #[allow(clippy::never_loop)]
    for _ in 0..4 {
        // Burst phase

        burst(c).await;

        for _ in 0..3 {
            while cooldown(c, StandardStep) - player(c).gcd as u32 > 1000 {
                filler_prio(c).await;
            }
            filler_standard(c).await;
        }

        while cooldown(c, TechnicalStep) - player(c).gcd as u32 > 1000 {
            pretech_prio(c).await;
        }
    }

    burst(c).await;
}

impl JobAi for DncAi {
    fn next(
        &mut self,
        event: &SimEvent,
        queue: &mut RadixEventQueue<SimEvent>,
        actor: ActorHandle,
        time: u32,
    ) -> bool {
        // check for per-event warnings
        if let SimEvent::Event(Event::Status(StatusEvent {
            kind: StatusEventKind::FallOff,
            status,
            source,
            ..
        })) = event
        {
            if *source == actor.id {
                static CHECK: &[StatusEffect] = &[
                    FAN_DANCE_3,
                    FAN_DANCE_4,
                    LAST_DANCE_READY,
                    DANCE_OF_THE_DAWN_READY,
                    FINISHING_MOVE_READY,
                    STARFALL,
                    FLOURISH_FLOW,
                    FLOURISH_SYMM,
                    SILKEN_FLOW,
                    SILKEN_SYMM,
                    FLOURISH_FINISH,
                ];

                if CHECK.contains(status) {
                    eprintln!("[warning] {} has fallen off.", status.name);
                }
            }
        }

        self.coro
            .resume(ResumeCtx {
                event,
                queue,
                actor,
                time,
            })
            .is_none()
    }
}
