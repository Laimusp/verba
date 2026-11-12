// Оконное приложение: без окна консоли при запуске из автозапуска и Проводника.
// Команды `verba key set` и т. п. пишут в консоль терминала, откуда их запустили (cli.rs).
#![windows_subsystem = "windows"]

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = if args.is_empty() { verba::app::run() } else { verba::cli::run(&args) };
    std::process::exit(code);
}
