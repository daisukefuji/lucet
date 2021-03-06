use crate::context::Context;
use crate::instance::{
    FaultDetails, Instance, State, TerminationDetails, CURRENT_INSTANCE, HOST_CTX,
};
use crate::trapcode::{TrapCode, TrapCodeType};
use failure::Error;
use lazy_static::lazy_static;
use libc::{c_int, c_void, siginfo_t};
use nix::sys::signal::{
    pthread_sigmask, raise, sigaction, SaFlags, SigAction, SigHandler, SigSet, SigmaskHow, Signal,
};
use std::sync::Mutex;

lazy_static! {
    // TODO: work out an alternative to this that is signal-safe for `reraise_host_signal_in_handler`
    static ref LUCET_SIGNAL_STATE: Mutex<Option<SignalState>> = Mutex::new(None);
}

/// The value returned by
/// [`Instance.signal_handler`](struct.Instance.html#structfield.signal_handler) to determine the
/// outcome of a handled signal.
pub enum SignalBehavior {
    /// Use default behavior, which switches back to the host with `State::Fault` populated.
    Default,
    /// Override default behavior and cause the instance to continue.
    Continue,
    /// Override default behavior and cause the instance to terminate.
    Terminate,
}

pub type SignalHandler =
    dyn Fn(&Instance, &TrapCode, libc::c_int, *const siginfo_t, *const c_void) -> SignalBehavior;

pub fn signal_handler_none(
    _inst: &Instance,
    _trapcode: &TrapCode,
    _signum: libc::c_int,
    _siginfo_ptr: *const siginfo_t,
    _ucontext_ptr: *const c_void,
) -> SignalBehavior {
    SignalBehavior::Default
}

impl Instance {
    pub(crate) fn with_signals_on<F, R>(&mut self, f: F) -> Result<R, Error>
    where
        F: FnOnce(&mut Instance) -> Result<R, Error>,
    {
        // setup signal stack for this thread
        let guest_sigstack = SigStack::new(
            self.alloc.slot().sigstack,
            SigStackFlags::empty(),
            libc::SIGSTKSZ,
        );
        let saved_sigstack = unsafe { sigaltstack(&guest_sigstack).expect("sigaltstack succeeds") };

        let mut ostate = LUCET_SIGNAL_STATE.lock().unwrap();
        if let Some(ref mut state) = *ostate {
            state.counter += 1;
        } else {
            unsafe {
                setup_guest_signal_state(&mut ostate);
            }
        }
        drop(ostate);

        // run the body
        let res = f(self);

        let mut ostate = LUCET_SIGNAL_STATE.lock().unwrap();
        let counter_zero = if let Some(ref mut state) = *ostate {
            state.counter -= 1;
            if state.counter == 0 {
                unsafe {
                    // restore the host signal stack
                    sigaltstack(&saved_sigstack).expect("sigaltstack succeeds");
                    restore_host_signal_state(state);
                }
                true
            } else {
                false
            }
        } else {
            panic!("signal handlers weren't installed at instance exit");
        };
        if counter_zero {
            *ostate = None;
        }

        res
    }
}

/// Signal handler installed during instance execution.
///
/// This function is only designed to handle signals that are the direct result of execution of a
/// hardware instruction from the faulting WASM thread. It thus safely assumes the signal is
/// directed specifically at this thread (i.e. not a different thread or the process as a whole).
extern "C" fn handle_signal(signum: c_int, siginfo_ptr: *mut siginfo_t, ucontext_ptr: *mut c_void) {
    let signal = Signal::from_c_int(signum).expect("signum is a valid signal");
    if !(signal == Signal::SIGBUS
        || signal == Signal::SIGSEGV
        || signal == Signal::SIGILL
        || signal == Signal::SIGFPE)
    {
        panic!("unexpected signal in guest signal handler: {:?}", signal);
    }
    assert!(!siginfo_ptr.is_null(), "siginfo must not be null");

    // Safety: when using a SA_SIGINFO sigaction, the third argument can be cast to a `ucontext_t`
    // pointer per the manpage
    let ctx = unsafe {
        (ucontext_ptr as *mut libc::ucontext_t)
            .as_ref()
            .expect("ucontext must not be null")
    };
    let rip = ctx.uc_mcontext.gregs[libc::REG_RIP as usize] as *const c_void;

    let switch_to_host = CURRENT_INSTANCE.with(|current_instance| {
        let mut current_instance = current_instance.borrow_mut();

        if current_instance.is_none() {
            // If there is no current instance, we've caught a signal raised by a thread that's not
            // running a lucet instance. Restore the host signal handler and reraise the signal,
            // then return if the host handler returns
            unsafe {
                reraise_host_signal_in_handler(signal, signum, siginfo_ptr, ucontext_ptr);
            }
            // don't try context-switching
            return false;
        }

        // Safety: the memory pointed to by CURRENT_INSTANCE should be a valid instance. This is not
        // a trivial property, but relies on the compiler not emitting guest programs that can
        // overwrite the instance.
        let inst = unsafe {
            current_instance
                .as_mut()
                .expect("current instance exists")
                .as_mut()
        };

        let trapcode = inst
            .module
            .lookup_trapcode(rip)
            // if we couldn't find a code in the manifest, return an unknown trapcode that will be
            // converted to a fatal trap when we switch back to the host
            .unwrap_or(TrapCode {
                ty: TrapCodeType::Unknown,
                tag: 0,
            });

        let behavior = (inst.signal_handler)(inst, &trapcode, signum, siginfo_ptr, ucontext_ptr);
        match behavior {
            SignalBehavior::Continue => {
                // return to the guest context without making any modifications to the instance
                false
            }
            SignalBehavior::Terminate => {
                // set the state before jumping back to the host context
                inst.state = State::Terminated {
                    details: TerminationDetails::Signal,
                };
                true
            }
            SignalBehavior::Default => {
                // record the fault and jump back to the host context
                inst.state = State::Fault {
                    details: FaultDetails {
                        // fatal field is set false here by default - have to wait until
                        // `verify_trap_safety` to have enough information to determine whether trap was
                        // fatal. It is not signal safe to access some of the required information.
                        fatal: false,
                        trapcode: trapcode,
                        rip_addr: rip as usize,
                        rip_addr_details: None,
                    },
                    // safety: pointer is checked for null at the top of the function, and the
                    // manpage guarantees that a siginfo_t will be passed as the second argument
                    siginfo: unsafe { *siginfo_ptr },
                    context: *ctx,
                };
                true
            }
        }
    });

    if switch_to_host {
        HOST_CTX.with(|host_ctx| unsafe {
            Context::set_from_signal(&*host_ctx.get())
                .expect("can successfully switch back to the host context");
        });
        unreachable!()
    }
}

struct SignalState {
    counter: usize,
    saved_sigbus: SigAction,
    saved_sigfpe: SigAction,
    saved_sigill: SigAction,
    saved_sigsegv: SigAction,
}

// raw pointers in the saved types
unsafe impl Send for SignalState {}

unsafe fn setup_guest_signal_state(ostate: &mut Option<SignalState>) {
    let mut masked_signals = SigSet::empty();
    masked_signals.add(Signal::SIGBUS);
    masked_signals.add(Signal::SIGFPE);
    masked_signals.add(Signal::SIGILL);
    masked_signals.add(Signal::SIGSEGV);

    // setup signal handlers
    let sa = SigAction::new(
        SigHandler::SigAction(handle_signal),
        SaFlags::SA_RESTART | SaFlags::SA_SIGINFO | SaFlags::SA_ONSTACK,
        masked_signals,
    );
    let saved_sigbus = sigaction(Signal::SIGBUS, &sa).expect("sigaction succeeds");
    let saved_sigfpe = sigaction(Signal::SIGFPE, &sa).expect("sigaction succeeds");
    let saved_sigill = sigaction(Signal::SIGILL, &sa).expect("sigaction succeeds");
    let saved_sigsegv = sigaction(Signal::SIGSEGV, &sa).expect("sigaction succeeds");

    *ostate = Some(SignalState {
        counter: 1,
        saved_sigbus,
        saved_sigfpe,
        saved_sigill,
        saved_sigsegv,
    });
}

unsafe fn restore_host_signal_state(state: &mut SignalState) {
    // restore signal handlers
    sigaction(Signal::SIGBUS, &state.saved_sigbus).expect("sigaction succeeds");
    sigaction(Signal::SIGFPE, &state.saved_sigfpe).expect("sigaction succeeds");
    sigaction(Signal::SIGILL, &state.saved_sigill).expect("sigaction succeeds");
    sigaction(Signal::SIGSEGV, &state.saved_sigsegv).expect("sigaction succeeds");
}

unsafe fn reraise_host_signal_in_handler(
    sig: Signal,
    signum: libc::c_int,
    siginfo_ptr: *mut libc::siginfo_t,
    ucontext_ptr: *mut c_void,
) {
    let saved_handler = {
        // TODO: avoid taking a mutex here, probably by having some static muts just for this
        // function
        if let Some(ref state) = *LUCET_SIGNAL_STATE.lock().unwrap() {
            match sig {
                Signal::SIGBUS => state.saved_sigbus.clone(),
                Signal::SIGFPE => state.saved_sigfpe.clone(),
                Signal::SIGILL => state.saved_sigill.clone(),
                Signal::SIGSEGV => state.saved_sigsegv.clone(),
                sig => panic!(
                    "unexpected signal in reraise_host_signal_in_handler: {:?}",
                    sig
                ),
            }
        } else {
            // this case is very fishy; it can arise when the last lucet instance spins down and
            // uninstalls the lucet handlers while a signal handler is running on this thread, but
            // before taking the mutex above. The theory is that if this has happened, the host
            // handler has been reinstalled, so we shouldn't end up back here if we reraise

            // unmask the signal to reraise; we don't have to restore it because the handler will return
            // after this. If it signals again between here and now, that's a double fault and the
            // process is going to die anyway
            let mut unmask = SigSet::empty();
            unmask.add(sig);
            pthread_sigmask(SigmaskHow::SIG_UNBLOCK, Some(&unmask), None)
                .expect("pthread_sigmask succeeds");
            // if there's no current signal state, just re-raise and hope for the best
            raise(sig).expect("raise succeeds");
            return;
        }
    };

    match saved_handler.handler() {
        SigHandler::SigDfl => {
            // reinstall default signal handler and reraise the signal; this should terminate the
            // program
            sigaction(sig, &saved_handler).expect("sigaction succeeds");
            let mut unmask = SigSet::empty();
            unmask.add(sig);
            pthread_sigmask(SigmaskHow::SIG_UNBLOCK, Some(&unmask), None)
                .expect("pthread_sigmask succeeds");
            raise(sig).expect("raise succeeds");
        }
        SigHandler::SigIgn => {
            // don't do anything; if we hit this case, whatever program is hosting us is almost
            // certainly doing something wrong, because our set of signals requires intervention to
            // proceed
            return;
        }
        SigHandler::Handler(f) => {
            // call the saved handler directly so there is no altstack confusion
            f(signum)
        }
        SigHandler::SigAction(f) => {
            // call the saved handler directly so there is no altstack confusion
            f(signum, siginfo_ptr, ucontext_ptr)
        }
    }
}

////////////////////////////////////////////////////////////////////////////////////////////////////
// A collection of wrappers that will be upstreamed to the `nix` crate eventually.
////////////////////////////////////////////////////////////////////////////////////////////////////

use bitflags::bitflags;

#[derive(Copy, Clone)]
pub struct SigStack {
    stack: libc::stack_t,
}

impl SigStack {
    pub fn new(sp: *mut libc::c_void, flags: SigStackFlags, size: libc::size_t) -> SigStack {
        let mut stack = unsafe { std::mem::uninitialized::<libc::stack_t>() };
        stack.ss_sp = sp;
        stack.ss_flags = flags.bits();
        stack.ss_size = size;
        SigStack { stack }
    }

    pub fn flags(&self) -> SigStackFlags {
        SigStackFlags::from_bits_truncate(self.stack.ss_flags)
    }
}

impl AsRef<libc::stack_t> for SigStack {
    fn as_ref(&self) -> &libc::stack_t {
        &self.stack
    }
}

impl AsMut<libc::stack_t> for SigStack {
    fn as_mut(&mut self) -> &mut libc::stack_t {
        &mut self.stack
    }
}

bitflags! {
    pub struct SigStackFlags: libc::c_int {
        const SS_ONSTACK = libc::SS_ONSTACK;
        const SS_DISABLE = libc::SS_DISABLE;
    }
}

pub unsafe fn sigaltstack(ss: &SigStack) -> nix::Result<SigStack> {
    let mut oldstack = std::mem::uninitialized::<libc::stack_t>();

    let res = libc::sigaltstack(
        &ss.stack as *const libc::stack_t,
        &mut oldstack as *mut libc::stack_t,
    );

    nix::errno::Errno::result(res).map(|_| SigStack { stack: oldstack })
}
