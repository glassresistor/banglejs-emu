use std::{
    borrow::Borrow,
    env, fs,
    ops::{Deref, DerefMut},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use log::{debug, info, trace};
use wasmer::{
    Extern, Function, FunctionEnv, FunctionType, Instance, Memory, MemoryType, Module, Pages,
    RuntimeError, Store, Type, TypedFunction, Value,
};
use wasmer_wasi::{import_object_for_all_wasi_versions, WasiState};

const BTN1: usize = 17;

fn main() -> anyhow::Result<()> {
    env_logger::init();

    let wasm_bytes = fs::read(env::args().nth(1).unwrap()).unwrap();
    let store_arc = Arc::new(Mutex::new(Store::default()));
    let mut store = store_arc.lock().unwrap();
    let store = store.deref_mut();
    let module = Module::new(store, wasm_bytes)?;

    let mut wasi_state_builder = WasiState::new("espruino");

    let wasi_env = wasi_state_builder.finalize(store)?;
    let mut import_object = import_object_for_all_wasi_versions(store, &wasi_env.env);

    let flash = Arc::new(Mutex::new(vec![255u8; 1 << 23]));
    let pins = Arc::new(Mutex::new(vec![false; 48]));

    pins.lock().unwrap()[BTN1] = true;

    let env_name = |s: &str| ("env".to_owned(), s.to_owned());

    #[derive(Clone, Debug)]
    struct Env {
        instance: Arc<Mutex<Option<Instance>>>,
    }
    let instance_env = FunctionEnv::new(
        store,
        Env {
            instance: Arc::new(Mutex::new(None)),
        },
    );

    fn js_handle_io(store: &mut Store, instance: &Instance) -> anyhow::Result<()> {
        let get_device: TypedFunction<(), i32> = instance
            .exports
            .get_typed_function(store, "jshGetDeviceToTransmit")?;
        let get_char: TypedFunction<i32, i32> = instance
            .exports
            .get_typed_function(store, "jshGetCharToTransmit")?;

        loop {
            let device = get_device.call(store)?;
            if device == 0 {
                println!();
                break Ok(());
            }
            let ch = char::from_u32(get_char.call(store, device)? as _).unwrap();
            print!("{ch}");
        }
    }

    import_object.extend([
        (
            env_name("jsHandleIO"),
            Extern::Function(Function::new_with_env(
                store,
                &instance_env,
                FunctionType::new([], []),
                {
                    let store = Arc::clone(&store_arc);
                    move |env, _| {
                        debug!("jsHandleIO");

                        let instance = env.data().instance.lock().unwrap();
                        let instance = instance.as_ref().unwrap();
                        let mut store = store.lock().unwrap();
                        let store = store.deref_mut();

                        js_handle_io(store, instance).unwrap();

                        Ok(vec![])
                    }
                },
            )),
        ),
        (
            env_name("hwFlashRead"),
            Extern::Function(Function::new(
                store,
                FunctionType::new([Type::I32], [Type::I32]),
                {
                    let flash = Arc::clone(&flash);
                    move |args| {
                        trace!("hwFlashRead {args:?}");
                        match args[0] {
                            Value::I32(ind) => {
                                Ok(vec![Value::I32(flash.lock().unwrap()[ind as usize] as i32)])
                            }
                            _ => Err(RuntimeError::new("bad type")),
                        }
                    }
                },
            )),
        ),
        (
            env_name("hwFlashWritePtr"),
            Extern::Function(Function::new_with_env(
                store,
                &wasi_env.env,
                FunctionType::new([Type::I32, Type::I32, Type::I32], []),
                {
                    let flash = Arc::clone(&flash);
                    let store = Arc::clone(&store_arc);
                    move |env, args| {
                        trace!("hwFlashWritePtr {args:?}");
                        let flash_addr = args[0].unwrap_i32();
                        let base = args[1].unwrap_i32();
                        let len = args[2].unwrap_i32();

                        let mut flash = flash.lock().unwrap();
                        let dst = &mut flash[flash_addr as usize..][..len as usize];
                        env.data()
                            .memory_view(store.lock().unwrap().deref())
                            .read(base as u64, dst)
                            .unwrap();
                        trace!("writing at {flash_addr}: {dst:?}");
                        Ok(vec![])
                    }
                },
            )),
        ),
        (
            env_name("hwGetPinValue"),
            Extern::Function(Function::new(
                store,
                FunctionType::new([Type::I32], [Type::I32]),
                {
                    let pins = Arc::clone(&pins);
                    move |args| {
                        debug!("hwGetPinValue {args:?}");
                        match args[0] {
                            Value::I32(ind) => {
                                Ok(vec![Value::I32(pins.lock().unwrap()[ind as usize] as i32)])
                            }
                            _ => Err(RuntimeError::new("bad type")),
                        }
                    }
                },
            )),
        ),
        (
            env_name("hwSetPinValue"),
            Extern::Function(Function::new(
                store,
                FunctionType::new([Type::I32, Type::I32], []),
                {
                    let pins = Arc::clone(&pins);
                    move |args| {
                        debug!("hwSetPinValue {args:?}");
                        match (&args[0], &args[1]) {
                            (Value::I32(ind), Value::I32(val)) => {
                                pins.lock().unwrap()[*ind as usize] = *val != 0;
                                Ok(vec![])
                            }
                            _ => Err(RuntimeError::new("bad type")),
                        }
                    }
                },
            )),
        ),
        (
            env_name("nowMillis"),
            Extern::Function(Function::new(
                store,
                FunctionType::new([], [Type::F32]),
                |_| {
                    trace!("nowMillis");
                    Ok(vec![Value::F32(
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_secs_f32()
                            * 1000.0,
                    )])
                },
            )),
        ),
    ]);

    import_object.extend([(
        env_name("memory"),
        Extern::Memory(
            Memory::new(
                store,
                MemoryType {
                    minimum: Pages(1 << 2),
                    maximum: Some(Pages(1 << 2)),
                    shared: false,
                },
            )
            .unwrap(),
        ),
    )]);

    let instance = Instance::new(store, &module, &import_object)?;
    let memory = instance.exports.get_memory("memory")?;
    wasi_env.data_mut(store).set_memory(memory.clone());
    *instance_env.as_mut(store).instance.lock().unwrap() = Some(instance.clone());

    let js_init: TypedFunction<(), ()> = instance.exports.get_typed_function(&store, "jsInit")?;
    let js_idle: TypedFunction<(), i32> = instance.exports.get_typed_function(&store, "jsIdle")?;
    let js_send_pin_watch_event: TypedFunction<i32, ()> = instance
        .exports
        .get_typed_function(&store, "jsSendPinWatchEvent")?;
    let js_gfx_get_ptr: TypedFunction<i32, i32> =
        instance.exports.get_typed_function(&store, "jsGfxGetPtr")?;

    fn draw_screen(
        store: &mut Store,
        memory: &Memory,
        get: &TypedFunction<i32, i32>,
    ) -> anyhow::Result<()> {
        let mut buf0 = vec![0u8; 66];
        let mut buf1 = vec![0u8; 66];
        let memory_view = memory.view(&store);
        for y in (0..176).step_by(2) {
            let base0 = get.call(store, y)?;
            let base1 = get.call(store, y + 1)?;
            memory_view.read(base0 as u64, &mut buf0)?;
            memory_view.read(base1 as u64, &mut buf1)?;

            fn get3(x: usize, buf: &[u8]) -> u8 {
                let bit = x * 3;
                let byte = bit >> 3;
                ((buf[byte] >> (bit & 7))
                    | if (bit & 7) <= 5 {
                        0
                    } else {
                        buf[byte + 1] << (8 - (bit & 7))
                    })
                    & 7
            }

            for x in 0..176 {
                let c0 = get3(x, &buf0);
                let c1 = get3(x, &buf1);
                print!("\x1b[{};{}m\u{2584}", 40 + c0, 30 + c1);
            }
            println!("\x1b[m");
        }
        Ok(())
    }

    fn js_push_string<T, B>(store: &mut Store, instance: &Instance, chars: T) -> anyhow::Result<()>
    where
        B: Borrow<u8>,
        T: IntoIterator<Item = B>,
    {
        let js_push_char: TypedFunction<(i32, i32), ()> = instance
            .exports
            .get_typed_function(&store, "jshPushIOCharEvent")?;
        for ch in chars {
            js_push_char.call(store, 21, *ch.borrow() as i32)?;
        }
        Ok(())
    }

    info!("==== init");
    js_init.call(store)?;
    js_send_pin_watch_event.call(store, BTN1 as i32)?;
    js_handle_io(store, &instance)?;

    js_push_string(store, &instance, b"console.log(17);LED1.set()\n")?;

    for step in 0..10 {
        info!("==== step {step}");
        let ret = js_idle.call(store)?;
        info!("-> {ret:?}");
        js_handle_io(store, &instance)?;
    }

    draw_screen(store, memory, &js_gfx_get_ptr)?;

    Ok(())
}
