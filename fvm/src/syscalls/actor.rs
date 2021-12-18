use crate::kernel::{ExecutionError, SyscallError};
use crate::syscalls::context::Context;
use crate::Kernel;
use wasmtime::{Caller, Trap};

pub fn resolve_address(
    mut caller: Caller<'_, impl Kernel>,
    addr_off: u32, // Address
    addr_len: u32,
) -> Result<(i32, u64), Trap> {
    let (k, mem) = caller.kernel_and_memory()?;
    let addr = mem.read_address(addr_off, addr_len)?;
    match k.resolve_address(&addr)? {
        Some(id) => Ok((0, id)),
        None => Ok((-1, 0)),
    }
}

pub fn get_actor_code_cid(
    mut caller: Caller<'_, impl Kernel>,
    addr_off: u32, // Address
    addr_len: u32,
    obuf_off: u32, // Cid
    obuf_len: u32,
) -> Result<i32, Trap> {
    let (k, mut mem) = caller.kernel_and_memory()?;
    let addr = mem.read_address(addr_off, addr_len)?;
    match k.get_actor_code_cid(&addr)? {
        Some(typ) => {
            let obuf = mem.try_slice_mut(obuf_off, obuf_len)?;
            typ.write_bytes(obuf).map_err(ExecutionError::from)?;
            Ok(0)
        }
        None => Ok(-1),
    }
}

/// Generates a new actor address, and writes it into the supplied output buffer.
///
/// The output buffer must be at least 21 bytes long, which is the length of a
/// class 2 address (protocol-generated actor address). This will change in the
/// future when we introduce class 4 addresses to accommodate larger hashes.
///
/// TODO this method will be merged with create_actor in the near future.
pub fn new_actor_address(
    mut caller: Caller<'_, impl Kernel>,
    obuf_off: u32, // Address (out)
    obuf_len: u32,
) -> Result<u32, Trap> {
    if obuf_len < 21 {
        return Err(ExecutionError::from(SyscallError::from(
            "output buffer must have a minimum capacity of 21 bytes",
        ))
        .into());
    }

    let (k, mut mem) = caller.kernel_and_memory()?;
    let addr = k.new_actor_address()?;
    let bytes = addr.to_bytes();

    let len = bytes.len();
    if len > obuf_len as usize {
        return Err(ExecutionError::from(SyscallError::from(format!(
            "insufficient output buffer capacity; {} (new address) > {} (buffer capacity)",
            len, obuf_len
        )))
        .into());
    }

    let obuf = mem.try_slice_mut(obuf_off, obuf_len)?;
    obuf[..len].copy_from_slice(bytes.as_slice());
    Ok(len as u32)
}

pub fn create_actor(
    mut caller: Caller<'_, impl Kernel>,
    addr_off: u32, // Address
    addr_len: u32,
    typ_off: u32, // Cid
) -> Result<(), Trap> {
    let (k, mem) = caller.kernel_and_memory()?;
    let addr = mem.read_address(addr_off, addr_len)?;
    let typ = mem.read_cid(typ_off)?;
    k.create_actor(typ, &addr)
        .map_err(ExecutionError::from)
        .map_err(Trap::from)
}
