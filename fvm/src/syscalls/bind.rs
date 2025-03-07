// Copyright 2021-2023 Protocol Labs
// SPDX-License-Identifier: Apache-2.0, MIT
use std::mem;

use fvm_shared::error::ErrorNumber;
use fvm_shared::sys::SyscallSafe;
use wasmtime::{Caller, Linker, WasmTy};

use super::context::Memory;
use super::error::Abort;
use super::{charge_for_exec, update_gas_available, Context, InvocationData};
use crate::call_manager::backtrace;
use crate::kernel::{self, ExecutionError, Kernel, SyscallError};

/// Binds syscalls to a linker, converting the returned error according to the syscall convention:
///
/// 1. If the error is a syscall error, it's returned as the first return value.
/// 2. If the error is a fatal error, a Trap is returned.
pub(super) trait BindSyscall<Args, Ret, Func> {
    /// Bind a syscall to the linker.
    ///
    /// 1. The return type will be automatically adjusted to return `Result<u32, Trap>` where
    /// `u32` is the error code.
    /// 2. If the return type is non-empty (i.e., not `()`), an out-pointer will be prepended to the
    /// arguments for the return-value.
    ///
    /// By example:
    ///
    /// - `fn(u32) -> kernel::Result<()>` will become `fn(u32) -> Result<u32, Trap>`.
    /// - `fn(u32) -> kernel::Result<i64>` will become `fn(u32, u32) -> Result<u32, Trap>`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// mod my_module {
    ///     pub fn zero(mut context: Context<'_, impl Kernel>, arg: i32) -> crate::fvm::kernel::Result<i32> {
    ///         Ok(0)
    ///     }
    /// }
    /// let engine = wasmtime::Engine::default();
    /// let mut linker = wasmtime::Linker::new(&engine);
    /// linker.bind("my_module", "zero", my_module::zero);
    /// ```
    fn bind(
        &mut self,
        module: &'static str,
        name: &'static str,
        syscall: Func,
    ) -> anyhow::Result<&mut Self>;
}

/// ControlFlow is a general-purpose enum allowing us to pass syscall error up the
/// stack to the actor and treat error handling there (decide when to abort, etc).
pub enum ControlFlow<T> {
    Return(T),
    Error(SyscallError),
    Abort(Abort),
}

impl<T> From<ExecutionError> for ControlFlow<T> {
    fn from(value: ExecutionError) -> Self {
        match value {
            ExecutionError::Syscall(err) => ControlFlow::Error(err),
            ExecutionError::Fatal(err) => ControlFlow::Abort(Abort::Fatal(err)),
            ExecutionError::OutOfGas => ControlFlow::Abort(Abort::OutOfGas),
        }
    }
}

/// The helper trait used by `BindSyscall` to convert kernel results with execution errors into
/// results that can be handled by wasmtime. See the documentation on `BindSyscall` for details.
#[doc(hidden)]
pub trait IntoControlFlow: Sized {
    type Value: SyscallSafe;
    fn into_control_flow(self) -> ControlFlow<Self::Value>;
}

/// An uninhabited type. We use this in `abort` to make sure there's no way to return without
/// returning an error.
#[derive(Copy, Clone)]
#[doc(hidden)]
pub enum Never {}
unsafe impl SyscallSafe for Never {}

// Implementations for syscalls that always abort.
impl IntoControlFlow for Abort {
    type Value = Never;
    fn into_control_flow(self) -> ControlFlow<Self::Value> {
        ControlFlow::Abort(self)
    }
}

// Implementations for syscalls that can abort.
impl<T> IntoControlFlow for ControlFlow<T>
where
    T: SyscallSafe,
{
    type Value = T;
    fn into_control_flow(self) -> ControlFlow<Self::Value> {
        self
    }
}

// Implementations for normal syscalls.
impl<T> IntoControlFlow for kernel::Result<T>
where
    T: SyscallSafe,
{
    type Value = T;
    fn into_control_flow(self) -> ControlFlow<Self::Value> {
        match self {
            Ok(value) => ControlFlow::Return(value),
            Err(e) => match e {
                ExecutionError::Syscall(err) => ControlFlow::Error(err),
                ExecutionError::OutOfGas => ControlFlow::Abort(Abort::OutOfGas),
                ExecutionError::Fatal(err) => ControlFlow::Abort(Abort::Fatal(err)),
            },
        }
    }
}

fn memory_and_data<'a, K: Kernel>(
    caller: &'a mut Caller<'_, InvocationData<K>>,
) -> (&'a mut Memory, &'a mut InvocationData<K>) {
    let memory_handle = caller.data().memory;
    let (mem, data) = memory_handle.data_and_store_mut(caller);
    (Memory::new(mem), data)
}

macro_rules! charge_syscall_gas {
    ($kernel:expr) => {
        let charge = $kernel.price_list().on_syscall();
        $kernel
            .charge_gas(&charge.name, charge.compute_gas)
            .map_err(Abort::from_error_as_fatal)?;
    };
}

// Unfortunately, we can't implement this for _all_ functions. So we implement it for functions of up to 6 arguments.
macro_rules! impl_bind_syscalls {
    ($($t:ident)*) => {
        #[allow(non_snake_case)]
        impl<$($t,)* Ret, K, Func> BindSyscall<($($t,)*), Ret, Func> for Linker<InvocationData<K>>
        where
            K: Kernel,
            Func: Fn(Context<'_, K> $(, $t)*) -> Ret + Send + Sync + 'static,
            Ret: IntoControlFlow,
           $($t: WasmTy+SyscallSafe,)*
        {
            fn bind(
                &mut self,
                module: &'static str,
                name: &'static str,
                syscall: Func,
            ) -> anyhow::Result<&mut Self> {
                if mem::size_of::<Ret::Value>() == 0 {
                    // If we're returning a zero-sized "value", we return no value therefore and expect no out pointer.
                    self.func_wrap(module, name, move |mut caller: Caller<'_, InvocationData<K>> $(, $t: $t)*| {
                        charge_for_exec(&mut caller)?;

                        let (mut memory, mut data) = memory_and_data(&mut caller);
                        charge_syscall_gas!(data.kernel);

                        let ctx = Context{kernel: &mut data.kernel, memory: &mut memory};
                        let out = syscall(ctx $(, $t)*).into_control_flow();

                        let result = match out {
                            ControlFlow::Return(_) => {
                                log::trace!("syscall {}::{}: ok", module, name);
                                data.last_error = None;
                                Ok(0)
                            },
                            ControlFlow::Error(err) => {
                                let code = err.1;
                                log::trace!("syscall {}::{}: fail ({})", module, name, code as u32);
                                data.last_error = Some(backtrace::Cause::from_syscall(module, name, err));
                                Ok(code as u32)
                            },
                            ControlFlow::Abort(abort) => Err(abort.into()),
                        };

                        update_gas_available(&mut caller)?;

                        result
                    })
                } else {
                    // If we're returning an actual value, we need to write it back into the wasm module's memory.
                    self.func_wrap(module, name, move |mut caller: Caller<'_, InvocationData<K>>, ret: u32 $(, $t: $t)*| {
                        charge_for_exec(&mut caller)?;

                        let (mut memory, mut data) = memory_and_data(&mut caller);
                        charge_syscall_gas!(data.kernel);

                        // We need to check to make sure we can store the return value _before_ we do anything.
                        if (ret as u64) > (memory.len() as u64)
                            || memory.len() - (ret as usize) < mem::size_of::<Ret::Value>() {
                            let code = ErrorNumber::IllegalArgument;
                            data.last_error = Some(backtrace::Cause::from_syscall(module, name, SyscallError(format!("no space for return value"), code)));
                            return Ok(code as u32);
                        }

                        let ctx = Context{kernel: &mut data.kernel, memory: &mut memory};
                        let result = match syscall(ctx $(, $t)*).into_control_flow() {
                            ControlFlow::Return(value) => {
                                log::trace!("syscall {}::{}: ok", module, name);
                                unsafe {
                                    // We're writing into a user-specified pointer, so avoid
                                    // derefering it as it may not be aligned.
                                    (memory.as_mut_ptr().offset(ret as isize) as *mut Ret::Value).write_unaligned(value);
                                }
                                data.last_error = None;
                                Ok(0)
                            },
                            ControlFlow::Error(err) => {
                                let code = err.1;
                                log::trace!("syscall {}::{}: fail ({})", module, name, code as u32);
                                data.last_error = Some(backtrace::Cause::from_syscall(module, name, err));
                                Ok(code as u32)
                            },
                            ControlFlow::Abort(abort) => Err(abort.into()),
                        };

                        update_gas_available(&mut caller)?;

                        result
                    })
                }
            }
        }
    }
}

impl_bind_syscalls!();
impl_bind_syscalls!(A);
impl_bind_syscalls!(A B);
impl_bind_syscalls!(A B C);
impl_bind_syscalls!(A B C D);
impl_bind_syscalls!(A B C D E);
impl_bind_syscalls!(A B C D E F);
impl_bind_syscalls!(A B C D E F G);
impl_bind_syscalls!(A B C D E F G H);
