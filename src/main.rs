// Оконное приложение: без окна консоли при запуске из автозапуска и Проводника.
// Команды `verba key set` и т. п. пишут в консоль терминала, откуда их запустили (cli.rs).
#![windows_subsystem = "windows"]

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.as_slice() {
        // руками: окно настроек и трей
        [] => verba::app::run(false),
        // из автозапуска (задача планировщика): только трей
        [a] if a == verba::autostart::TRAY_ARG => verba::app::run(true),
        _ => verba::cli::run(&args),
    };
    std::process::exit(code);
}
