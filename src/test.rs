
use crate::{prelude::*, register::regid};
use std::{cell::Cell, rc::Rc, sync::Arc};

#[cfg(windows)]
const TARGET: &str = "notepad.exe";

#[cfg(unix)]
const TARGET: &str = "cat";

async fn loop_util(
    state: &UEventState,
    exit: impl Fn(&Arc<dyn UDbgAdaptor>, &UEvent) -> bool,
) -> Arc<dyn UDbgAdaptor> {
    state
        .loop_util(|target, event| {
            info!(
                "[event]~{}:{} {event}",
                target.pid(),
                target.base().event_tid.get()
            );
            match event {
                UEvent::Exception { first, code } => {
                    let pc = state
                        .context()
                        .register()
                        .unwrap()
                        .get("_pc")
                        .unwrap()
                        .as_int();
                    info!("  PC: {pc:x} {:?}", target.get_symbol_string(pc));
                }
                UEvent::ProcessCreate => {
                    info!("  {}", target.base().image_path);
                }
                _ => {}
            }
            exit(target, event)
        })
        .await
}

#[test]
fn debug() -> anyhow::Result<()> {
    flexi_logger::Logger::try_with_env_or_str("info")?
        .use_utc()
        .start()?;

    let arg = "!!!---";
    let mut engine = crate::os::DefaultEngine::default();
    engine.create(TARGET, None, &[arg]).expect("create target");

    #[derive(Default)]
    struct State {
        entry_hitted: Cell<bool>,
        fopen_hitted: Cell<bool>,
        hwbp_hitted: Cell<bool>,
    }
    let st = Rc::new(State::default());
    let ds = st.clone();
    engine.task_loop(DebugTask::from(|state: UEventState| async move {
        let state = &state;
        let target = loop_util(state, |_, e| matches!(e, UEvent::InitBp)).await;
        info!("target path: {}", target.base().image_path);

        info!("initbp occured");
        let main = target.get_main_module().unwrap();
        info!(
            "main module: {} entry: {:x} +{:x}",
            main.data().path,
            main.data().entry_point(),
            main.data().entry,
        );
        target.addbp(main.data().entry_point()).expect("add bp");
        assert_eq!(
            &target
                .read_value::<BpInsn>(main.data().entry_point())
                .unwrap(),
            BP_INSN
        );
        info!("breakpoint added");

        loop_util(state, |target, event| match event {
            UEvent::Breakpoint(bp) => {
                info!("entrypoint bp occured");
                let regs = state.context().register().unwrap();
                assert_eq!(
                    regs.get_reg(regid::COMM_REG_PC).unwrap().as_int(),
                    bp.address() as _
                );
                bp.remove().unwrap();

                ds.entry_hitted.set(true);
                target
                    .add_bp(
                        target
                            .get_address_by_symbol("kernel32!CreateFileW")
                            .or_else(|| target.get_address_by_symbol("libc!open"))
                            .or_else(|| target.get_address_by_symbol("libc!__open64"))
                            .unwrap()
                            .into(),
                    )
                    .expect("add bp");
                true
            }
            _ => false,
        })
        .await;

        loop_util(state, |target, event| match event {
            UEvent::Breakpoint(bp) => {
                let regs = state.context().register().unwrap();
                assert_eq!(
                    regs.get_reg(regid::COMM_REG_PC).unwrap().as_int(),
                    bp.address()
                );
                let arg1;
                let argstr;
                #[cfg(windows)]
                {
                    arg1 = regs
                        .get_reg(match std::env::consts::ARCH {
                            "aarch64" => regid::ARM64_REG_X0,
                            "x86_64" => regid::X86_REG_RCX,
                            _ => unreachable!(),
                        })
                        .unwrap()
                        .as_int();
                    let arg1 = target.read_wstring(arg1, None).unwrap_or_default();
                    argstr = arg1.strip_suffix(".txt").unwrap_or(&arg1).to_string();
                }
                #[cfg(unix)]
                {
                    arg1 = regs
                        .get_reg(match std::env::consts::ARCH {
                            "aarch64" => regid::ARM64_REG_X0,
                            "x86_64" => regid::X86_REG_RDI,
                            _ => unreachable!(),
                        })
                        .unwrap()
                        .as_int();
                    argstr = target.read_utf8(arg1, None).unwrap_or_default();
                }
                info!("fopen: 0x{arg1:x} {argstr}");
                if argstr == arg {
                    ds.fopen_hitted.set(true);
                    target
                        .add_bp((arg1, HwbpType::Access).into())
                        .expect("add hwbp");
                    bp.remove().unwrap();
                    true
                } else {
                    false
                }
            }
            _ => false,
        })
        .await;

        loop_util(state, |_, event| match event {
            UEvent::Breakpoint(bp) => {
                assert!(bp.get_type().is_hard());
                ds.hwbp_hitted.set(true);
                info!("HWBP occured");
                bp.remove().unwrap();
                true
            }
            _ => false,
        })
        .await;

        target.kill().expect("kill");

        loop_util(state, |_, _| false).await;
    }))?;
    assert!(st.entry_hitted.get());
    assert!(st.fopen_hitted.get());
    assert!(st.hwbp_hitted.get());

    Ok(())
}