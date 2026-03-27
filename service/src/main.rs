mod bindings {
    wit_bindgen::generate!({
        world: "cron-service",
        path: "../wit",
    });
}

use bindings::cosmonic::uptime_monitor::cron;

fn main() {
    let mut tick = 0u64;
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
        cron::poll_all();
        tick += 1;
        // Prune old history every 60 seconds
        if tick % 60 == 0 {
            cron::prune();
        }
    }
}
