use std::{
    cell::{Cell, UnsafeCell},
    future::Future,
    marker::{PhantomData, PhantomPinned},
    mem::{self, MaybeUninit},
    ops::AsyncFnOnce,
    pin::Pin,
    task::{Context, Poll, Waker},
};

use xivc_core::{
    job::{self},
    world::{queue::RadixEventQueue, Action},
};

use crate::{ActorHandle, CdEndEvent, SimEvent};

pub struct JobAiCoro {
    future: Pin<Box<dyn CtrlFuture>>,
}

struct ControllerFuture<F: Future> {
    value: UnsafeCell<Option<OpaqueResumeCtx>>,
    borrowed: Cell<bool>,
    future: Option<F>,
    _pin: PhantomPinned,
}

trait CtrlFuture {
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()>;

    fn set_value(self: Pin<&mut Self>, val: OpaqueResumeCtx);
}

impl<F: Future<Output = ()>> CtrlFuture for ControllerFuture<F> {
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        unsafe {
            let this = self.get_unchecked_mut();

            if let Some(v) = this.future.as_mut() {
                match Pin::new_unchecked(v).poll(cx) {
                    Poll::Pending => {
                        if this.borrowed.get() {
                            panic!();
                        }
                        Poll::Pending
                    }
                    Poll::Ready(..) => {
                        this.future = None;
                        Poll::Ready(())
                    }
                }
            } else {
                Poll::Ready(())
            }
        }
    }

    fn set_value(self: Pin<&mut Self>, val: OpaqueResumeCtx) {
        if self.borrowed.get() {
            panic!();
        }

        unsafe {
            *self.value.get() = Some(val);
            self.borrowed.set(true);
        }
    }
}

impl JobAiCoro {
    pub fn new<F>(f: F) -> Self
    where
        F: for<'c> AsyncFnOnce(Controller<'c>) + 'static,
    {
        let mut future = Box::pin(ControllerFuture {
            value: UnsafeCell::new(None),
            borrowed: Cell::new(false),
            future: None,
            _pin: PhantomPinned,
        });

        let value = &future.value as *const _;
        let borrowed = &future.borrowed as *const _;

        let controller = Controller {
            borrowed,
            value,
            phantom: PhantomData,
        };

        let inner = f(controller);

        unsafe {
            future.as_mut().get_unchecked_mut().future = Some(inner);
        }

        Self { future }
    }

    pub fn resume(&mut self, ctx: ResumeCtx<'_>) -> Option<()> {
        let mut cx = Context::from_waker(Waker::noop());

        let opaque_ctx: OpaqueResumeCtx = unsafe { mem::transmute(ctx) };
        self.future.as_mut().set_value(opaque_ctx);
        match self.future.as_mut().poll(&mut cx) {
            Poll::Ready(_) => None,
            Poll::Pending => Some(()),
        }
    }
}

#[repr(C)]
pub struct ResumeCtx<'w> {
    pub time: u32,
    pub event: &'w SimEvent,
    pub queue: &'w mut RadixEventQueue<SimEvent>,
    pub actor: ActorHandle<'w>,
}

#[repr(C)]
struct OpaqueResumeCtx(MaybeUninit<ResumeCtx<'static>>);

impl OpaqueResumeCtx {
    fn as_resume_ctx_mut(&mut self) -> &mut ResumeCtx {
        unsafe { mem::transmute(self) }
    }
    fn as_resume_ctx_ref(&self) -> &ResumeCtx {
        unsafe { mem::transmute(self) }
    }
}

pub struct Controller<'w> {
    value: *const UnsafeCell<Option<OpaqueResumeCtx>>,
    borrowed: *const Cell<bool>,
    phantom: PhantomData<&'w ()>,
}

impl<'w> Controller<'w> {
    pub fn ctx_mut(&mut self) -> &mut ResumeCtx {
        unsafe {
            assert!((*self.borrowed).get());
            (*UnsafeCell::raw_get(self.value))
                .as_mut()
                .unwrap()
                .as_resume_ctx_mut()
        }
    }

    pub fn ctx(&self) -> &ResumeCtx {
        unsafe {
            assert!((*self.borrowed).get());
            (*UnsafeCell::raw_get(self.value))
                .as_ref()
                .unwrap()
                .as_resume_ctx_ref()
        }
    }
}

impl<'w> Controller<'w> {
    pub fn yield_wait(&mut self) -> impl Future<Output = ()> + '_ {
        struct Impl {
            finished: bool,
        }

        unsafe {
            UnsafeCell::raw_get(self.value).replace(None);
            (*self.borrowed).set(false);
        }

        impl Future for Impl {
            type Output = ();

            fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
                if self.finished {
                    Poll::Ready(())
                } else {
                    self.finished = true;
                    Poll::Pending
                }
            }
        }

        Impl { finished: false }
    }

    pub async fn wait_action(&mut self, action: job::Action) {
        let player = self.ctx().actor.actor.player.as_ref().unwrap();

        let cd = action
            .cd_info()
            .map(|(g, cd, ch)| (g, player.cooldowns.get(g).unwrap().cd_until(cd, ch)));

        let mut wait_gcd = action.gcd() && player.gcd > 0;
        let mut wait_lock = player.lock > 0;
        let mut wait_cd = cd.filter(|v| v.1 > 0).map(|v| v.0);

        loop {
            match self.ctx().event {
                SimEvent::CdEnd(CdEndEvent::Gcd) => wait_gcd = false,
                SimEvent::CdEnd(CdEndEvent::Lock) => wait_lock = false,
                SimEvent::CdEnd(CdEndEvent::JobCd(e)) if Some(*e) == wait_cd => wait_cd = None,
                _ => (),
            }
            if !wait_gcd && !wait_lock && wait_cd.is_none() {
                break;
            }
            self.yield_wait().await;
        }
    }

    pub async fn wait_gcd(&mut self) {
        let player = self.ctx().actor.actor.player.as_ref().unwrap();

        let mut wait_lock = player.lock > 0;
        let mut wait_gcd = player.gcd > 0;

        loop {
            match self.ctx().event {
                SimEvent::CdEnd(CdEndEvent::Gcd) => wait_gcd = false,
                SimEvent::CdEnd(CdEndEvent::Lock) => wait_lock = false,
                _ => (),
            }
            if !wait_lock && !wait_gcd {
                break;
            }
            self.yield_wait().await;
        }
    }

    pub async fn wait_before_gcd(&mut self, before: u16) {
        let ctx = self.ctx();
        let player = ctx.actor.actor.player.as_ref().unwrap();
        let time = ctx.time;

        let target = if player.gcd > before {
            let target = (player.gcd - before) as u32 + time;
            self.ctx_mut().queue.push(target, SimEvent::Other);
            target
        } else {
            return;
        };

        loop {
            let ctx = self.ctx();
            // if the event is an Other event and at the correct time.
            if matches!(ctx.event, SimEvent::Other) && target == ctx.time {
                break;
            }
            self.yield_wait().await;
        }
    }

    pub async fn wait_lock(&mut self) {
        let player = self.ctx().actor.actor.player.as_ref().unwrap();

        let mut wait_lock = player.lock > 0;

        loop {
            if matches!(self.ctx().event, SimEvent::CdEnd(CdEndEvent::Lock)) {
                wait_lock = false;
            }
            if !wait_lock {
                break;
            }
            self.yield_wait().await;
        }
    }

    pub async fn cast(&mut self, action: job::Action) {
        self.wait_action(action).await;

        let ctx = self.ctx_mut();
        let time = ctx.time;
        let actor = ctx.actor.id;
        ctx.queue
            .push(time, SimEvent::StartCast(actor, Action::Job(action)));

        loop {
            self.yield_wait().await;
            match self.ctx().event {
                SimEvent::CastSnap(id, ac) if *id == actor && *ac == Action::Job(action) => return,
                _ => (),
            }
        }
    }

    pub async fn wait(&mut self, delay: u32) {
        let ctx = self.ctx_mut();

        let target_time = ctx.time + delay;

        ctx.queue.push(target_time, SimEvent::Other);

        loop {
            self.yield_wait().await;
            let ctx = self.ctx();
            if matches!(ctx.event, SimEvent::Other) && ctx.time == target_time {
                break;
            }
        }
    }
}
