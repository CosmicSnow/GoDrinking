// Both development and packaged Windows apps are GUI processes.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

fn main() {
    golive_app::run();
}
