//! Detached containers, end to end (`run::detached`, Linux): a container
//! that outlives the process that made it, its resources held by its
//! monitor, and let go by `__cleanup` once it ends.
//!
//! `harness = false`: this binary is its own `shipyard`. Its `main` calls
//! every entry point first (the containers' init and monitors are this
//! binary, and so is their `__cleanup`), and its `launch` subcommand is the
//! "CLI": a child process that calls `detached` and exits, as `run -d`
//! will. The tests then look at what's left once it has gone.
//!
//! Root tests use the default bridge network with a published port, in a
//! network namespace of the tests' own (made first thing: whatever they
//! make goes with it); a normal user's, `--network none`; pasta's tests ask
//! for pasta (the real one, or a fake first on the "CLI"'s `PATH`). All use
//! an image from the fixture registry.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::field_reassign_with_default,
    clippy::too_many_lines,
    reason = "a panic in a test is a clear failure; a request is made as callers make it; an \
              end-to-end test checks a container's whole life in order"
)]

use std::process::ExitCode;

fn main() -> ExitCode {
    shipyard_engine::entry_points(shipyard_engine::Hooks {
        init_logging: || {},
        refuse_privileged_start: || Ok(()),
    });
    let args: Vec<String> = std::env::args().skip(1).collect();
    #[cfg(target_os = "linux")]
    if args.first().map(String::as_str) == Some("launch") {
        return linux::launch(&args[1..]);
    }
    #[cfg(target_os = "linux")]
    if args.first().map(String::as_str) == Some("foreground") {
        return linux::foreground(&args[1..]);
    }
    #[cfg(target_os = "linux")]
    if args.first().map(String::as_str) == Some("serve") {
        return linux::serve(&args[1..]);
    }
    #[cfg(target_os = "linux")]
    if let Some(
        command @ ("start" | "start-abort" | "start-kill-init" | "rm" | "stop" | "attach"
        | "start-a" | "exec" | "exec-i" | "cp" | "connect-abort" | "disconnect-abort"),
    ) = args.first().map(String::as_str)
    {
        return linux::container_command(command, &args[1..]);
    }
    // As root, in a network namespace of the tests' own: the bridges and
    // firewall rules they make go with it.
    #[cfg(target_os = "linux")]
    if shipyard_test_support::is_root() {
        linux::own_network_namespace();
    }
    #[cfg(target_os = "linux")]
    let tests: Vec<(&str, fn())> = linux::TESTS.to_vec();
    #[cfg(not(target_os = "linux"))]
    let tests: Vec<(&str, fn())> = Vec::new();
    // Picked as libtest picks (a filter, `--exact`, `--skip`); none is
    // `#[ignore]`d, so `--include-ignored` runs them all.
    let tests: Vec<(&str, fn())> =
        shipyard_test_support::harness::select(&args, &tests, &[], false)
            .into_iter()
            .copied()
            .collect();
    if args.iter().any(|arg| arg == "--list") {
        for (name, _) in &tests {
            println!("{name}: test");
        }
        return ExitCode::SUCCESS;
    }
    println!("\nrunning {} tests", tests.len());
    let mut failed = Vec::new();
    for &(name, test) in &tests {
        print!("test {name} ... ");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let passed = std::panic::catch_unwind(test).is_ok();
        println!("{}", if passed { "ok" } else { "FAILED" });
        if !passed {
            failed.push(name);
        }
    }
    println!(
        "\ntest result: {}. {} passed; {} failed",
        if failed.is_empty() { "ok" } else { "FAILED" },
        tests.len() - failed.len(),
        failed.len()
    );
    if failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::os::unix::fs::PermissionsExt as _;
    use std::os::unix::process::ExitStatusExt as _;
    use std::path::{Path, PathBuf};
    use std::process::{Command, ExitCode, Output};
    use std::sync::Arc;
    use std::time::{Duration, Instant, SystemTime};

    use shipyard_engine::ErrorKind;
    use shipyard_engine::containers::{self, ContainerId, ContainerState, Record};
    use shipyard_engine::network::NetworkMode;
    use shipyard_engine::report::{Collect, Message, Reporter, WarningKind};
    use shipyard_engine::run::RunRequest;
    use shipyard_engine::{Engine, Paths};
    use shipyard_runtime::container as runtime;
    use shipyard_runtime::process::Signal;
    use shipyard_test_support::is_root;
    use shipyard_test_support::registry::{Scratch, fixture};

    /// The tests, in order.
    pub(super) const TESTS: &[(&str, fn())] = &[
        (
            "a_detached_container_outlives_its_caller_and_is_cleaned_up",
            a_detached_container_outlives_its_caller_and_is_cleaned_up,
        ),
        (
            "a_failed_start_leaves_a_created_container_holding_nothing",
            a_failed_start_leaves_a_created_container_holding_nothing,
        ),
        (
            "a_failed_start_keeps_what_its_last_start_left",
            a_failed_start_keeps_what_its_last_start_left,
        ),
        (
            "a_start_that_fails_with_its_init_alive_ends_it",
            a_start_that_fails_with_its_init_alive_ends_it,
        ),
        (
            "a_failure_before_create_leaves_a_created_record",
            a_failure_before_create_leaves_a_created_record,
        ),
        (
            "a_command_that_doesnt_exist_ends_127_holding_nothing",
            a_command_that_doesnt_exist_ends_127_holding_nothing,
        ),
        (
            "interface_sysctls_need_their_interface",
            interface_sysctls_need_their_interface,
        ),
        (
            "a_ctrl_c_before_create_undoes_everything",
            a_ctrl_c_before_create_undoes_everything,
        ),
        (
            "a_ctrl_c_between_create_and_start_waits",
            a_ctrl_c_between_create_and_start_waits,
        ),
        (
            "a_monitor_killed_leaves_what_the_next_run_sweeps",
            a_monitor_killed_leaves_what_the_next_run_sweeps,
        ),
        (
            "a_killed_monitors_container_is_settled_by_the_next_create",
            a_killed_monitors_container_is_settled_by_the_next_create,
        ),
        (
            "a_crashed_rm_container_is_removed",
            a_crashed_rm_container_is_removed,
        ),
        (
            "an_unreadable_container_is_removed_by_its_id_with_force",
            an_unreadable_container_is_removed_by_its_id_with_force,
        ),
        (
            "a_crashed_rm_container_isnt_started_again",
            a_crashed_rm_container_isnt_started_again,
        ),
        (
            "a_reboot_is_told_by_the_boot_id",
            a_reboot_is_told_by_the_boot_id,
        ),
        (
            "recovery_leaves_a_busy_container_alone",
            recovery_leaves_a_busy_container_alone,
        ),
        ("a_dead_start_is_settled", a_dead_start_is_settled),
        (
            "a_users_stop_is_recorded_and_a_start_forgets_it",
            a_users_stop_is_recorded_and_a_start_forgets_it,
        ),
        (
            "on_failure_restarts_up_to_its_max",
            on_failure_restarts_up_to_its_max,
        ),
        (
            "on_failure_leaves_a_clean_exit",
            on_failure_leaves_a_clean_exit,
        ),
        (
            "always_restarts_until_its_user_stops_it",
            always_restarts_until_its_user_stops_it,
        ),
        (
            "a_stop_while_restarting_ends_the_restarts",
            a_stop_while_restarting_ends_the_restarts,
        ),
        (
            "update_to_no_ends_the_restarts",
            update_to_no_ends_the_restarts,
        ),
        (
            "rm_refuses_a_restarting_container",
            rm_refuses_a_restarting_container,
        ),
        (
            "a_killed_restart_waiter_is_settled",
            a_killed_restart_waiter_is_settled,
        ),
        (
            "rm_with_a_restart_policy_is_refused",
            rm_with_a_restart_policy_is_refused,
        ),
        (
            "a_failed_restart_counts_as_a_failure",
            a_failed_restart_counts_as_a_failure,
        ),
        (
            "a_restart_keeps_the_latest_ten_starts_logs",
            a_restart_keeps_the_latest_ten_starts_logs,
        ),
        (
            "a_restart_leaves_the_open_cleanup_log",
            a_restart_leaves_the_open_cleanup_log,
        ),
        (
            "an_end_the_store_refused_is_retried",
            an_end_the_store_refused_is_retried,
        ),
        (
            "a_start_while_restarting_takes_over",
            a_start_while_restarting_takes_over,
        ),
        (
            "boot_starts_what_the_policies_say",
            boot_starts_what_the_policies_say,
        ),
        (
            "the_host_stop_is_graceful_and_not_a_users",
            the_host_stop_is_graceful_and_not_a_users,
        ),
        (
            "a_paused_container_stops_gracefully",
            a_paused_container_stops_gracefully,
        ),
        (
            "a_restart_waiting_for_its_lock_sees_the_host_stopping",
            a_restart_waiting_for_its_lock_sees_the_host_stopping,
        ),
        (
            "an_upgraded_binary_still_cleans_up",
            an_upgraded_binary_still_cleans_up,
        ),
        (
            "a_pasta_container_follows_its_init",
            a_pasta_container_follows_its_init,
        ),
        (
            "a_pasta_that_fails_leaves_nothing",
            a_pasta_that_fails_leaves_nothing,
        ),
        ("a_pasta_never_ready_is_ended", a_pasta_never_ready_is_ended),
        (
            "the_init_killed_while_pasta_starts_ends_it",
            the_init_killed_while_pasta_starts_ends_it,
        ),
        (
            "a_pasta_whose_launcher_was_killed_ends_with_the_clean_up",
            a_pasta_whose_launcher_was_killed_ends_with_the_clean_up,
        ),
        (
            "a_launcher_interrupted_ends_its_pasta",
            a_launcher_interrupted_ends_its_pasta,
        ),
        (
            "a_pasta_launcher_pins_no_host_mount",
            a_pasta_launcher_pins_no_host_mount,
        ),
        (
            "a_container_starts_again_with_its_files",
            a_container_starts_again_with_its_files,
        ),
        (
            "a_start_right_after_its_own_end_works",
            a_start_right_after_its_own_end_works,
        ),
        (
            "running_is_free_only_once_its_end_is_settled",
            running_is_free_only_once_its_end_is_settled,
        ),
        (
            "its_pastas_ports_are_free_once_its_end_is_settled",
            its_pastas_ports_are_free_once_its_end_is_settled,
        ),
        (
            "a_start_waits_for_its_last_clean_up",
            a_start_waits_for_its_last_clean_up,
        ),
        (
            "an_end_before_the_gate_records_no_exit",
            an_end_before_the_gate_records_no_exit,
        ),
        (
            "a_start_after_its_monitor_was_killed_works",
            a_start_after_its_monitor_was_killed_works,
        ),
        (
            "a_failed_start_keeps_the_containers_files",
            a_failed_start_keeps_the_containers_files,
        ),
        (
            "the_image_stays_with_its_container",
            the_image_stays_with_its_container,
        ),
        (
            "two_container_stores_share_an_image_store",
            two_container_stores_share_an_image_store,
        ),
        (
            "start_and_rm_never_interleave",
            start_and_rm_never_interleave,
        ),
        ("run_d_rm_leaves_nothing", run_d_rm_leaves_nothing),
        (
            "a_terminal_container_starts_from_a_script",
            a_terminal_container_starts_from_a_script,
        ),
        (
            "a_start_killed_before_its_end_leaves_it_startable",
            a_start_killed_before_its_end_leaves_it_startable,
        ),
        (
            "rm_beside_a_listing_of_the_areas",
            rm_beside_a_listing_of_the_areas,
        ),
        (
            "start_and_rm_beside_an_rm_containers_end",
            start_and_rm_beside_an_rm_containers_end,
        ),
        (
            "stop_ends_it_and_it_starts_again",
            stop_ends_it_and_it_starts_again,
        ),
        (
            "stop_kills_one_that_ignores_its_signal",
            stop_kills_one_that_ignores_its_signal,
        ),
        (
            "kill_reaches_one_a_stop_waits_on",
            kill_reaches_one_a_stop_waits_on,
        ),
        ("wait_gives_the_exit_code", wait_gives_the_exit_code),
        (
            "wait_reaches_an_rm_containers_end",
            wait_reaches_an_rm_containers_end,
        ),
        ("restart_stops_then_starts", restart_stops_then_starts),
        (
            "a_stop_killed_meanwhile_still_ends_it",
            a_stop_killed_meanwhile_still_ends_it,
        ),
        (
            "the_monitor_serves_while_it_waits_at_the_end",
            the_monitor_serves_while_it_waits_at_the_end,
        ),
        ("the_earliest_stop_wins", the_earliest_stop_wins),
        (
            "an_attach_client_cant_stop_it",
            an_attach_client_cant_stop_it,
        ),
        (
            "a_control_client_sees_no_output",
            a_control_client_sees_no_output,
        ),
        (
            "a_real_time_stop_signal_arrives",
            a_real_time_stop_signal_arrives,
        ),
        (
            "a_shorter_stop_goes_past_a_longer_one",
            a_shorter_stop_goes_past_a_longer_one,
        ),
        (
            "waiters_never_keep_a_stop_out",
            waiters_never_keep_a_stop_out,
        ),
        ("logs_cover_every_start", logs_cover_every_start),
        ("logs_follow_until_it_ends", logs_follow_until_it_ends),
        (
            "logs_follow_ends_when_it_stops",
            logs_follow_ends_when_it_stops,
        ),
        (
            "attach_gets_its_output_and_its_end",
            attach_gets_its_output_and_its_end,
        ),
        ("start_a_misses_nothing", start_a_misses_nothing),
        ("exec_runs_as_a_user_by_name", exec_runs_as_a_user_by_name),
        (
            "exec_without_input_reads_nothing",
            exec_without_input_reads_nothing,
        ),
        (
            "exec_of_a_missing_command_is_127",
            exec_of_a_missing_command_is_127,
        ),
        ("cp_copies_by_name", cp_copies_by_name),
        (
            "cp_reaches_a_stopped_container",
            cp_reaches_a_stopped_container,
        ),
        (
            "a_held_area_refuses_a_view_and_a_start",
            a_held_area_refuses_a_view_and_a_start,
        ),
        (
            "a_failed_start_keeps_what_its_user_stopped",
            a_failed_start_keeps_what_its_user_stopped,
        ),
        (
            "a_stop_of_a_dead_start_stays_stopped",
            a_stop_of_a_dead_start_stays_stopped,
        ),
        ("cp_into_a_created_container", cp_into_a_created_container),
        (
            "cp_reaches_a_stopped_containers_volumes",
            cp_reaches_a_stopped_containers_volumes,
        ),
        (
            "cp_of_a_read_only_container_copies_out_only",
            cp_of_a_read_only_container_copies_out_only,
        ),
        ("a_start_waits_for_a_copy", a_start_waits_for_a_copy),
        (
            "a_start_waits_for_a_copy_into_one_that_ended",
            a_start_waits_for_a_copy_into_one_that_ended,
        ),
        (
            "cp_reaches_a_stopped_rootfs_container",
            cp_reaches_a_stopped_rootfs_container,
        ),
        ("cp_as_a_container_ends", cp_as_a_container_ends),
        (
            "a_foreground_run_is_a_container",
            a_foreground_run_is_a_container,
        ),
        (
            "a_terminal_foreground_run_is_a_container",
            a_terminal_foreground_run_is_a_container,
        ),
        (
            "a_terminal_foreground_run_needs_a_terminal_to_type_on",
            a_terminal_foreground_run_needs_a_terminal_to_type_on,
        ),
        (
            "a_foreground_run_with_rm_is_gone_when_it_returns",
            a_foreground_run_with_rm_is_gone_when_it_returns,
        ),
        (
            "input_ends_at_your_end_of_file",
            input_ends_at_your_end_of_file,
        ),
        (
            "a_forwarded_signal_ends_it_with_its_code",
            a_forwarded_signal_ends_it_with_its_code,
        ),
        (
            "the_container_outlives_its_run",
            the_container_outlives_its_run,
        ),
        ("a_slow_run_misses_nothing", a_slow_run_misses_nothing),
        (
            "a_closed_output_ends_it_as_a_pipe_would",
            a_closed_output_ends_it_as_a_pipe_would,
        ),
        (
            "a_stopped_cp_keeps_its_owners",
            a_stopped_cp_keeps_its_owners,
        ),
        (
            "a_stopped_container_cant_be_reached",
            a_stopped_container_cant_be_reached,
        ),
        (
            "events_tell_a_containers_life",
            events_tell_a_containers_life,
        ),
        (
            "a_policy_restarts_events_count_it",
            a_policy_restarts_events_count_it,
        ),
        (
            "a_settled_die_is_dated_when_it_ended",
            a_settled_die_is_dated_when_it_ended,
        ),
        (
            "an_oom_kill_says_oom_then_die",
            an_oom_kill_says_oom_then_die,
        ),
        (
            "an_end_its_clean_up_missed_is_said_once",
            an_end_its_clean_up_missed_is_said_once,
        ),
        ("rm_f_says_kill_die_destroy", rm_f_says_kill_die_destroy),
        ("a_passing_check_is_healthy", a_passing_check_is_healthy),
        (
            "a_check_reads_its_record_alone",
            a_check_reads_its_record_alone,
        ),
        (
            "failing_checks_make_it_unhealthy",
            failing_checks_make_it_unhealthy,
        ),
        (
            "a_check_out_of_time_fails_and_leaves_nothing",
            a_check_out_of_time_fails_and_leaves_nothing,
        ),
        (
            "the_start_period_forgives_failures",
            the_start_period_forgives_failures,
        ),
        (
            "the_companion_goes_with_each_start",
            the_companion_goes_with_each_start,
        ),
        (
            "a_paused_container_isnt_checked",
            a_paused_container_isnt_checked,
        ),
        (
            "a_check_that_leaves_a_process_still_answers",
            a_check_that_leaves_a_process_still_answers,
        ),
        ("a_dead_pasta_is_said", a_dead_pasta_is_said),
        (
            "a_pasta_ending_with_its_init_isnt_a_loss",
            a_pasta_ending_with_its_init_isnt_a_loss,
        ),
        (
            "a_pasta_that_ends_is_said_with_its_code",
            a_pasta_that_ends_is_said_with_its_code,
        ),
        ("the_images_check_is_used", the_images_check_is_used),
        (
            "flags_tune_or_replace_the_images_check",
            flags_tune_or_replace_the_images_check,
        ),
        (
            "tuning_a_check_there_isnt_is_said",
            tuning_a_check_there_isnt_is_said,
        ),
        (
            "an_images_none_or_bad_check_and_its_shell",
            an_images_none_or_bad_check_and_its_shell,
        ),
        (
            "a_check_under_way_at_the_end_counts_for_nothing",
            a_check_under_way_at_the_end_counts_for_nothing,
        ),
        (
            "rm_of_an_end_unwatched_says_its_die",
            rm_of_an_end_unwatched_says_its_die,
        ),
        (
            "a_users_starts_keep_the_latest_ten_starts_logs",
            a_users_starts_keep_the_latest_ten_starts_logs,
        ),
        (
            "a_start_killed_after_recording_itself_keeps_its_history",
            a_start_killed_after_recording_itself_keeps_its_history,
        ),
        (
            "an_end_settled_by_a_pass_forgets_the_start_kept_aside",
            an_end_settled_by_a_pass_forgets_the_start_kept_aside,
        ),
        (
            "an_exit_command_that_never_started_is_said",
            an_exit_command_that_never_started_is_said,
        ),
        (
            "rename_port_and_sockets_refuse_by_kind",
            rename_port_and_sockets_refuse_by_kind,
        ),
        (
            "connect_and_disconnect_refuse_by_kind",
            connect_and_disconnect_refuse_by_kind,
        ),
        (
            "a_cut_short_connect_is_finished_by_its_rerun",
            a_cut_short_connect_is_finished_by_its_rerun,
        ),
    ];

    /// Moves this process (single-threaded still: its children follow) into
    /// a network namespace of its own, its loopback up (the fixture
    /// registry listens there).
    pub(super) fn own_network_namespace() {
        nix::sched::unshare(nix::sched::CloneFlags::CLONE_NEWNET).expect("a network namespace");
        shipyard_network::route::Rtnetlink::new()
            .expect("rtnetlink")
            .change(1, shipyard_network::route::LinkChange::Up(true))
            .expect("lo up");
    }

    /// The "CLI" of a foreground run: `foreground [--rm] [-i] [-t]
    /// [--no-sig-proxy] [--stop-signal SIGNAL] IMAGE COMMAND…` runs it as
    /// `shipyard run` does (its output this process's), then says `ran=CODE name=NAME` on stderr and
    /// exits with its code (or dies from the signal passed on that ended
    /// it, as `shipyard run` does); or `error=KIND: MESSAGE`, exit 125.
    pub(super) fn foreground(args: &[String]) -> ExitCode {
        let mut request = RunRequest::default();
        let mut options = shipyard_engine::run::Foreground {
            sig_proxy: true,
            detach_keys: None,
        };
        let mut auto_remove = false;
        let mut rest = args.iter();
        while let Some(arg) = rest.next() {
            match arg.as_str() {
                "--rm" => auto_remove = true,
                "-i" => request.streams.interactive = true,
                "-t" => request.streams.tty = true,
                "--no-sig-proxy" => options.sig_proxy = false,
                "--stop-signal" => {
                    request.stop_signal = rest.next().unwrap().parse().unwrap();
                }
                image => {
                    request.image = Some(image.to_owned());
                    request.command = rest.by_ref().map(Into::into).collect();
                }
            }
        }
        request.network = vec![NetworkMode::None];
        let engine = Engine::new(Paths::from_env(), Arc::new(Stderr) as Arc<dyn Reporter>);
        match shipyard_engine::run::attached(&engine, request, auto_remove, options) {
            Ok(ran) => {
                eprintln!("ran={} name={}", ran.code, ran.name);
                // As `shipyard run`: ended by a signal passed on, from it too.
                if let Some(signal) = ran.interrupted {
                    shipyard_runtime::process::die_from(signal);
                }
                ExitCode::from(ran.code)
            }
            Err(error) => {
                eprintln!("error={:?}: {error:#}", error.kind());
                ExitCode::from(125)
            }
        }
    }

    /// `start REFERENCE`, `rm REFERENCE` (forced), `stop REFERENCE
    /// [SECONDS]`, `attach REFERENCE`, `start-a REFERENCE` or `exec
    /// REFERENCE USER|- COMMAND…`, as the CLI's (what the container or the
    /// command writes comes out as it does): prints `code=…` or `ran=…` for
    /// the last three, then `ok`, or `error=KIND: MESSAGE`.
    pub(super) fn container_command(command: &str, args: &[String]) -> ExitCode {
        let engine = Engine::new(Paths::from_env(), Arc::new(Stderr) as Arc<dyn Reporter>);
        let done = match command {
            "start" => shipyard_engine::run::start(&engine, &args[0]).map(drop),
            // Its init killed once it's made, before its gate opens: the
            // start fails.
            "start-kill-init" => shipyard_engine::run::start_with(&engine, &args[0], |id| {
                let root = engine.paths().runtime_root().unwrap().to_owned();
                let pid = runtime::state(&root, id.as_str()).unwrap().pid.unwrap();
                nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(pid),
                    nix::sys::signal::Signal::SIGKILL,
                )
                .unwrap();
                let deadline = Instant::now() + Duration::from_secs(10);
                while runtime::status(&root, id.as_str()).unwrap() == Some(runtime::Status::Created)
                    && Instant::now() < deadline
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
            })
            .map(drop),
            // Killed once the container is made, before its gate opens.
            "start-abort" => {
                shipyard_engine::run::start_with(&engine, &args[0], |_| std::process::abort())
                    .map(drop)
            }
            // `connect-abort NETWORK CONTAINER`: killed once its record says
            // the network, before the container joins (F57).
            "connect-abort" => shipyard_engine::run::connect_with(
                &engine,
                &shipyard_engine::run::ConnectRequest::new(&args[0], &args[1]),
                || std::process::abort(),
            ),
            // `disconnect-abort NETWORK CONTAINER`: killed once its record no
            // longer says the network, before the container leaves.
            "disconnect-abort" => {
                shipyard_engine::run::disconnect_with(&engine, &args[0], &args[1], false, || {
                    std::process::abort()
                })
            }
            "stop" => shipyard_engine::run::stop(
                &engine,
                &args[0],
                shipyard_engine::run::StopOptions {
                    timeout: args
                        .get(1)
                        .map(|seconds| Duration::from_secs(seconds.parse().unwrap())),
                    ..Default::default()
                },
            )
            .map(drop),
            "rm" => shipyard_engine::run::remove(&engine, &args[0], true).map(drop),
            "attach" => shipyard_engine::attach::to_container(&engine, &args[0], &quiet())
                .map(|code| println!("code={code:?}")),
            "start-a" => shipyard_engine::run::start_attached(&engine, &args[0], &quiet())
                .map(|code| println!("code={code:?}")),
            "exec" | "exec-i" => shipyard_engine::exec::in_container(
                &engine,
                &args[0],
                &shipyard_engine::exec::ExecRequest {
                    args: args[2..].iter().map(Into::into).collect(),
                    user: (args[1] != "-").then(|| args[1].clone()),
                    interactive: command == "exec-i",
                    ..Default::default()
                },
            )
            .map(|ran| println!("ran={ran:?}")),
            // `cp FROM TO` (as the CLI's), its streams this process's.
            "cp" => cp(&engine, &args[0], &args[1]).map(drop),
            other => panic!("no such command: {other}"),
        };
        match done {
            Ok(()) => {
                println!("ok");
                ExitCode::SUCCESS
            }
            Err(error) => {
                println!("error={:?}: {error:#}", error.kind());
                ExitCode::FAILURE
            }
        }
    }

    /// Attaching without input, with the usual detach keys.
    fn quiet() -> shipyard_engine::attach::AttachOptions {
        shipyard_engine::attach::AttachOptions {
            detach_keys: shipyard_runtime::attach::DetachKeys::default_keys(),
            stdin: false,
            held: false,
        }
    }

    /// The "CLI": `launch [--volume V] [--publish P] [--sysctl K=V]
    /// [--network pasta|none] [--rm] [--linger] [--kill-init | --break-start |
    /// --interrupt] IMAGE COMMAND…` calls `detached` (with `between` doing
    /// what's asked) and prints `id=ID`, or `error=KIND: MESSAGE`; with
    /// `--linger`, then `children=none` if it has no child left (or which it
    /// has); then exits.
    pub(super) fn launch(args: &[String]) -> ExitCode {
        let mut request = RunRequest::default();
        let mut between = Between::Nothing;
        let mut network = None;
        let mut linger = false;
        let mut auto_remove = false;
        let mut rest = args.iter();
        while let Some(arg) = rest.next() {
            match arg.as_str() {
                "--volume" => request.volume.push(rest.next().unwrap().clone()),
                "--publish" => request
                    .publish
                    .extend(shipyard_network::ports::parse_publish(rest.next().unwrap()).unwrap()),
                "--sysctl" => {
                    let (key, value) = rest.next().unwrap().split_once('=').unwrap();
                    request.sysctl.push((key.to_owned(), value.to_owned()));
                }
                "--network" => {
                    network = Some(match rest.next().unwrap().as_str() {
                        "pasta" => NetworkMode::Pasta,
                        "none" => NetworkMode::None,
                        other => panic!("no network mode {other} here"),
                    });
                }
                "--linger" => linger = true,
                "--rm" => auto_remove = true,
                "--restart" => request.restart = rest.next().unwrap().parse().unwrap(),
                // A health check of the script given, quick: every 300 ms
                // (in the start period too), 2 s to answer, the retries
                // given (or 3).
                "--health" => {
                    request.health = Some(shipyard_engine::health::HealthCheck {
                        interval_ms: 300,
                        start_interval_ms: 300,
                        timeout_ms: 2000,
                        ..shipyard_engine::health::HealthCheck::shell(rest.next().unwrap())
                    });
                }
                "--no-healthcheck" => {
                    request.health = Some(shipyard_engine::health::HealthCheck::disabled());
                }
                // Retries alone: the image's check, tuned.
                "--tune-retries" => {
                    request.health = Some(shipyard_engine::health::HealthCheck {
                        test: Vec::new(),
                        retries: rest.next().unwrap().parse().unwrap(),
                        ..shipyard_engine::health::HealthCheck::shell("")
                    });
                }
                "--health-retries" => {
                    request.health.as_mut().unwrap().retries =
                        rest.next().unwrap().parse().unwrap();
                }
                "--health-timeout-ms" => {
                    request.health.as_mut().unwrap().timeout_ms =
                        rest.next().unwrap().parse().unwrap();
                }
                "--health-start-period-ms" => {
                    request.health.as_mut().unwrap().start_period_ms =
                        rest.next().unwrap().parse().unwrap();
                }
                "--small-memory" => {
                    let mut resources = shipyard_runtime::resources::Resources::default();
                    resources.memory_max = Some(32 << 20);
                    resources.swap_max = Some(shipyard_runtime::resources::Limit::Value(0));
                    request.limits.resources = Ok(resources);
                }
                "--kill-init" => between = Between::KillInit,
                "--break-start" => between = Between::BreakStart,
                "--interrupt" => between = Between::Interrupt,
                "--abort-between" => between = Between::Abort,
                image => {
                    request.image = Some(image.to_owned());
                    request.command = rest.by_ref().map(Into::into).collect();
                }
            }
        }
        // Rootless, `none` unless asked: pasta's tests ask for it.
        match network {
            Some(mode) => request.network = vec![mode],
            None if !is_root() => request.network = vec![NetworkMode::None],
            None => {}
        }
        let engine = Engine::new(Paths::from_env(), Arc::new(Stderr) as Arc<dyn Reporter>);
        let runtime_root = engine.paths().runtime_root().unwrap().to_owned();
        let detached =
            shipyard_engine::run::detached_with(
                &engine,
                request,
                auto_remove,
                |id| match between {
                    Between::Nothing => {}
                    Between::KillInit => {
                        let pid = runtime::state(&runtime_root, id.as_str())
                            .unwrap()
                            .pid
                            .unwrap();
                        nix::sys::signal::kill(
                            nix::unistd::Pid::from_raw(pid),
                            nix::sys::signal::Signal::SIGKILL,
                        )
                        .unwrap();
                        // Gone before the start: wait for its monitor to see it.
                        let deadline = Instant::now() + Duration::from_secs(10);
                        while runtime::status(&runtime_root, id.as_str()).unwrap()
                            == Some(runtime::Status::Created)
                            && Instant::now() < deadline
                        {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                    }
                    Between::Interrupt => {
                        nix::sys::signal::raise(nix::sys::signal::Signal::SIGINT).unwrap();
                    }
                    // Killed outright, the container made and not started.
                    Between::Abort => std::process::abort(),
                    // Its start can't open the gate: it fails, the init alive.
                    Between::BreakStart => {
                        std::fs::remove_file(runtime_root.join(id.as_str()).join("exec.fifo"))
                            .unwrap();
                    }
                },
            );
        match detached {
            Ok(detached) => {
                println!("id={}", detached.id.as_str());
                // A caller that lives on: nothing `detached` started is its
                // child still (a zombie, or one it would have to reap).
                if linger {
                    match nix::sys::wait::waitpid(
                        nix::unistd::Pid::from_raw(-1),
                        Some(nix::sys::wait::WaitPidFlag::WNOHANG),
                    ) {
                        Err(nix::errno::Errno::ECHILD) => println!("children=none"),
                        other => {
                            let me = std::process::id().to_string();
                            let mine: Vec<String> = std::fs::read_dir("/proc")
                                .into_iter()
                                .flatten()
                                .flatten()
                                .filter_map(|entry| {
                                    let stat =
                                        std::fs::read_to_string(entry.path().join("stat")).ok()?;
                                    let ppid = stat
                                        .rsplit(')')
                                        .next()?
                                        .split_whitespace()
                                        .nth(1)?
                                        .to_owned();
                                    (ppid == me).then(|| {
                                        let line = std::fs::read(entry.path().join("cmdline"))
                                            .unwrap_or_default();
                                        String::from_utf8_lossy(&line).replace('\0', " ")
                                    })
                                })
                                .collect();
                            println!("children={other:?} {mine:?}");
                        }
                    }
                }
                ExitCode::SUCCESS
            }
            Err(error) => {
                println!("error={:?}: {error:#}", error.kind());
                ExitCode::FAILURE
            }
        }
    }

    /// `serve PID PORT REPLY`: answers `REPLY` to every connection on
    /// `PORT`, in the network namespace of process `PID` (a container's
    /// init), joining its user namespace first if it isn't this one's (a
    /// rootless container's: single-threaded, as this child is). Until
    /// killed.
    pub(super) fn serve(args: &[String]) -> ExitCode {
        use std::io::Write as _;
        use std::os::unix::fs::MetadataExt as _;
        let [pid, port, reply] = args else {
            return ExitCode::FAILURE;
        };
        let ns = |kind: &str| std::fs::File::open(format!("/proc/{pid}/ns/{kind}")).unwrap();
        let inode = |path: String| std::fs::metadata(path).unwrap().ino();
        if inode(format!("/proc/{pid}/ns/user")) != inode("/proc/self/ns/user".to_owned()) {
            nix::sched::setns(ns("user"), nix::sched::CloneFlags::CLONE_NEWUSER).unwrap();
        }
        nix::sched::setns(ns("net"), nix::sched::CloneFlags::CLONE_NEWNET).unwrap();
        let listener =
            std::net::TcpListener::bind(("0.0.0.0", port.parse::<u16>().unwrap())).unwrap();
        println!("listening");
        for connection in listener.incoming().flatten() {
            let mut connection = connection;
            let _ = connection.write_all(reply.as_bytes());
        }
        ExitCode::SUCCESS
    }

    /// What the "CLI" does between create and start.
    enum Between {
        Nothing,
        KillInit,
        Interrupt,
        BreakStart,
        Abort,
    }

    /// The "CLI"'s reporter: warnings on its stderr.
    struct Stderr;

    impl Reporter for Stderr {
        fn report(&self, message: Message) {
            eprintln!("{message:?}");
        }
    }

    /// One test's stores (image, containers, networks, a normal user's
    /// runtime directory) and its fixture image, removed at the end.
    struct Stores {
        scratch: Scratch,
        image: String,
        _registry: shipyard_test_support::registry::Serving,
    }

    impl Stores {
        fn new() -> Self {
            Self::serving(fixture())
        }

        /// Stores whose image has `runtime` as its config's `config`.
        fn with_image_config(runtime: &str) -> Self {
            Self::serving(shipyard_test_support::registry::fixture_with_config(
                runtime,
            ))
        }

        fn serving((registry, image): (shipyard_test_support::registry::Serving, String)) -> Self {
            let scratch = Scratch::new();
            for dir in ["containers", "networks", "xdg"] {
                std::fs::create_dir_all(scratch.0.join(dir)).unwrap();
            }
            for dir in ["containers", "xdg"] {
                std::fs::set_permissions(
                    scratch.0.join(dir),
                    std::fs::Permissions::from_mode(0o700),
                )
                .unwrap();
            }
            Self {
                scratch,
                image,
                _registry: registry,
            }
        }

        /// Its environment, for the "CLI" and this process's engine.
        fn env(&self) -> Vec<(&'static str, PathBuf)> {
            let mut env = vec![
                ("SHIPYARD_IMAGE_STORE", self.scratch.store()),
                ("SHIPYARD_CONTAINER_DIR", self.scratch.0.join("containers")),
                ("SHIPYARD_NETWORK_DIR", self.scratch.0.join("networks")),
            ];
            if !is_root() {
                env.push(("XDG_RUNTIME_DIR", self.scratch.0.join("xdg")));
            }
            env
        }

        /// This process's engine on the same stores.
        fn engine(&self) -> Engine {
            self.engine_collecting().0
        }

        /// [`engine`](Self::engine), keeping what it reports.
        fn engine_collecting(&self) -> (Engine, Arc<Collect>) {
            let seen = Arc::new(Collect::default());
            let mut paths = Paths::from_env()
                .with_image_store(self.scratch.store())
                .with_containers(self.scratch.0.join("containers"))
                .with_networks(self.scratch.0.join("networks"));
            if !is_root() {
                paths = paths.with_runtime_root(self.scratch.0.join("xdg/shipyard"));
            }
            let engine = Engine::new(paths, Arc::clone(&seen) as Arc<dyn Reporter>);
            (engine, seen)
        }

        /// Kills container `id`'s monitor outright (its container with it:
        /// its parent-death signal), and waits for every hold, and its init,
        /// to go: nothing records the end.
        fn kill_monitor(&self, id: &ContainerId) {
            let init = runtime::state(&self.runtime_root(), id.as_str())
                .unwrap()
                .pid
                .unwrap();
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(parent(init)),
                nix::sys::signal::Signal::SIGKILL,
            )
            .unwrap();
            let running = containers::testing::record_dir(&self.engine(), id)
                .unwrap()
                .join("running");
            eventually("the monitor's holds gone", || !locked(&running));
            // Its init too (its parent-death signal comes a moment later).
            eventually("its init gone", || {
                matches!(
                    runtime::peek_status(&self.runtime_root(), id.as_str()),
                    Ok(Some(runtime::Status::Stopped) | None)
                )
            });
        }

        /// Container `id`'s starts' log directories.
        fn starts(&self, id: &ContainerId) -> usize {
            names(&containers::testing::logs_dir(&self.engine(), id).unwrap()).len()
        }

        /// Container `id` (a crash loop, up): its next restart made to wait
        /// about 8 s (its last delay said to be 4 s, doubled after a short
        /// run), for a test to act in, once it's waiting.
        fn slow_restart(&self, id: &ContainerId) {
            containers::testing::update(&self.engine(), id, |record| {
                record.restart_delay_ms = 4000;
            })
            .unwrap();
            eventually("its restart waiting", || {
                matches!(self.state(id), ContainerState::Restarting { .. })
            });
        }

        /// `--network none --restart POLICY IMAGE sh -c SCRIPT`, launched:
        /// its ID.
        fn launch_restarting(&self, policy: &str, script: &str) -> ContainerId {
            Self::id(&self.launch(&[
                "--network",
                "none",
                "--restart",
                policy,
                self.image.as_str(),
                "sh",
                "-c",
                script,
            ]))
        }

        /// Waits for the process `pid` (a restart's waiter) to be gone.
        fn waiter_gone(pid: i32) {
            eventually("the waiter gone", || {
                !std::path::Path::new(&format!("/proc/{pid}")).exists()
            });
        }

        /// The `__cleanup` waiting to restart container `id` (its PID).
        fn restart_waiter(id: &ContainerId) -> i32 {
            std::fs::read_dir("/proc")
                .unwrap()
                .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<i32>().ok())
                .find(|pid| {
                    std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|line| {
                        let line = String::from_utf8_lossy(&line);
                        line.contains("__cleanup") && line.contains(id.as_str())
                    })
                })
                .expect("a waiting restart")
        }

        fn runtime_root(&self) -> PathBuf {
            self.engine().paths().runtime_root().unwrap().to_owned()
        }

        /// Runs the "CLI" (`program`, this binary unless given) with `args`.
        fn launch_with(&self, program: &Path, args: &[&str]) -> Output {
            let mut command = Command::new(program);
            command.arg("launch").args(args);
            for (key, value) in self.env() {
                command.env(key, value);
            }
            command.output().unwrap()
        }

        fn launch(&self, args: &[&str]) -> Output {
            self.launch_with(&std::env::current_exe().unwrap(), args)
        }

        /// A request for `command` in the fixture image, without a network
        /// (as root, each test's default network takes one of the twelve
        /// /16s a default network may have, its bridge left in the tests'
        /// namespace: a test that needs one asks).
        fn request(&self, command: &[&str]) -> RunRequest {
            let mut request = RunRequest::default();
            request.image = Some(self.image.clone());
            request.command = command.iter().map(Into::into).collect();
            request.network = vec![NetworkMode::None];
            request
        }

        /// The test binary's `foreground FLAGS… IMAGE COMMAND…` (as `shipyard
        /// run`), its output piped.
        fn foreground(&self, flags: &[&str], command: &[&str]) -> Command {
            let mut child = Command::new(std::env::current_exe().unwrap());
            child
                .arg("foreground")
                .args(flags)
                .arg(&self.image)
                .args(command);
            for (key, value) in self.env() {
                child.env(key, value);
            }
            child
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            child
        }

        /// The test binary's `start`, `start-abort`, `stop` or `rm` (as the
        /// CLI's), on `reference`, started with this test's environment.
        fn container_command(&self, command: &str, reference: &str) -> Command {
            let mut child = Command::new(std::env::current_exe().unwrap());
            child.args([command, reference]);
            for (key, value) in self.env() {
                child.env(key, value);
            }
            child.stdout(std::process::Stdio::piped());
            child
        }

        /// The upper (writable) directory of container `id`'s own area.
        fn upper(&self, id: &ContainerId) -> PathBuf {
            self.scratch
                .store()
                .join("containers")
                .join(shipyard_image::owned_area_name(id.as_str()))
                .join("upper")
        }

        /// Container `id`'s `running` file, opened (not locked).
        /// Whether something works on container `id` (its operation lock
        /// is held).
        fn worked_on(&self, id: &ContainerId) -> bool {
            let engine = self.engine();
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(
                    containers::testing::record_dir(&engine, id)
                        .unwrap()
                        .join("op.lock"),
                )
                .is_ok_and(|lock| lock.try_lock().is_err())
        }

        fn running_file(&self, id: &ContainerId) -> std::fs::File {
            let engine = self.engine();
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(
                    containers::testing::record_dir(&engine, id)
                        .unwrap()
                        .join("running"),
                )
                .unwrap()
        }

        /// Kills container `id` and waits for its end to be recorded and its
        /// runtime state to be gone (its clean-up has run).
        fn kill_and_wait(&self, id: &ContainerId) {
            runtime::kill(
                &self.runtime_root(),
                id.as_str(),
                shipyard_runtime::process::Signal::KILL,
            )
            .unwrap();
            eventually("the record's exit", || self.record(id).exit.is_some());
            eventually("its runtime state gone", || {
                runtime::status(&self.runtime_root(), id.as_str())
                    .unwrap()
                    .is_none()
            });
        }

        /// Runs the "CLI" with `args`, a fake `pasta` first on its `PATH`
        /// (`behaviour`: what it does when started, its `--help` aside).
        fn launch_with_fake_pasta(&self, behaviour: &str, args: &[&str]) -> Output {
            self.with_pasta_script(
                &format!(
                    "case \"$*\" in *--help*) echo '  -I, --ns-ifname NAME'; exit 0;; \
                     esac\n{behaviour}"
                ),
                args,
            )
            .output()
            .unwrap()
        }

        /// The "CLI" with `args`, a `pasta` first on its `PATH` that is the
        /// shell script `script`.
        fn with_pasta_script(&self, script: &str, args: &[&str]) -> Command {
            use std::os::unix::fs::PermissionsExt as _;
            let bin = self.scratch.0.join("fakebin");
            std::fs::create_dir_all(&bin).unwrap();
            let path = bin.join("pasta");
            std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            let search = format!(
                "{}:{}",
                bin.display(),
                std::env::var("PATH").unwrap_or_default()
            );
            let mut command = Command::new(std::env::current_exe().unwrap());
            command.arg("launch").args(args).env("PATH", search);
            for (key, value) in self.env() {
                command.env(key, value);
            }
            command
        }

        /// The "CLI" with `args`, its `pasta` the real one, started without
        /// a parent-death signal, as SELinux's `pasta_t` starts it: it
        /// doesn't die with its launcher.
        fn with_pasta_undying(&self, args: &[&str]) -> Command {
            let real = shipyard_network::pasta::find(std::env::var_os("PATH").as_deref()).unwrap();
            self.with_pasta_script(
                &format!("exec setpriv --pdeathsig clear {} \"$@\"", real.display()),
                args,
            )
        }

        /// The PID in the `pasta.pid` of a run directory not in `before`.
        fn pasta_pid(&self, before: &[String]) -> Option<i32> {
            let runs = self.runtime_root().join(".runs");
            names(&runs)
                .iter()
                .filter(|run| !before.contains(run))
                .find_map(|run| {
                    std::fs::read_to_string(runs.join(run).join("pasta.pid"))
                        .ok()?
                        .trim()
                        .parse()
                        .ok()
                })
        }

        /// The run directories left under the runtime root.
        fn run_dirs(&self) -> Vec<String> {
            names(&self.runtime_root().join(".runs"))
                .into_iter()
                .filter(|name| name != "lock")
                .collect()
        }

        /// The ID a launch printed.
        fn id(output: &Output) -> ContainerId {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let id = stdout
                .lines()
                .find_map(|line| line.strip_prefix("id="))
                .unwrap_or_else(|| {
                    panic!("no ID: {stdout}{}", String::from_utf8_lossy(&output.stderr))
                });
            ContainerId::parse(id).unwrap()
        }

        /// Every event in the journal (`SHIPYARD_EVENTS_DIR` unset: beside
        /// the container store), oldest first.
        fn events(&self) -> Vec<shipyard_engine::events::Event> {
            let options = shipyard_engine::events::EventsOptions {
                since: Some(SystemTime::UNIX_EPOCH),
                until: Some(SystemTime::now()),
                ..Default::default()
            };
            let mut read = Vec::new();
            shipyard_engine::events::read(&self.engine(), &options, |event| {
                read.push(event);
                Ok(())
            })
            .unwrap();
            read
        }

        /// The actions of the events about `id`, in the journal's order.
        fn actions(&self, id: &str) -> Vec<String> {
            self.events()
                .into_iter()
                .filter(|event| event.id == id)
                .map(|event| event.action)
                .collect()
        }

        fn record(&self, id: &ContainerId) -> Record {
            containers::testing::find(&self.engine(), id.as_str()).unwrap()
        }

        fn state(&self, id: &ContainerId) -> ContainerState {
            let engine = self.engine();
            containers::state(&engine, &self.record(id)).unwrap()
        }

        /// The files of the image store's writable areas.
        fn areas(&self) -> Vec<String> {
            names(&self.scratch.store().join("containers"))
                .into_iter()
                .filter(|name| name.starts_with("run-") || name.starts_with("ctr-"))
                .collect()
        }

        /// The default bridge network's lease files.
        fn leases(&self) -> Vec<PathBuf> {
            self.leases_on("bridge")
        }

        /// The network `network`'s lease files.
        fn leases_on(&self, network: &str) -> Vec<PathBuf> {
            std::fs::read_dir(self.scratch.0.join("networks").join(network).join("leases"))
                .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
                .unwrap_or_default()
        }

        /// The run directory of the run whose lease is `lease` (its `run`).
        fn run_dir(&self, lease: &Path) -> PathBuf {
            let lease: serde_json::Value =
                serde_json::from_slice(&std::fs::read(lease).unwrap()).unwrap();
            self.runtime_root()
                .join(".runs")
                .join(lease["run"].as_str().unwrap())
        }

        /// What `cleanup.log` says, if anything.
        fn cleanup_log(&self, id: &ContainerId) -> String {
            std::fs::read_to_string(
                self.scratch
                    .0
                    .join("containers")
                    .join(id.as_str())
                    .join("cleanup.log"),
            )
            .unwrap_or_default()
        }
    }

    /// The names in `dir`.
    fn names(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Process `pid`'s parent.
    fn parent(pid: i32) -> i32 {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        stat.rsplit(')')
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap()
    }

    /// Process `pid`'s blocked signals (`SigBlk` in its status, hex).
    fn blocked_signals(pid: i32) -> String {
        status_field(pid, "SigBlk:")
    }

    /// A field of process `pid`'s status.
    fn status_field(pid: i32, field: &str) -> String {
        std::fs::read_to_string(format!("/proc/{pid}/status"))
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix(field))
            .unwrap()
            .trim()
            .to_owned()
    }

    /// Process `pid`'s session.
    fn session(pid: i32) -> i32 {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        stat.rsplit(')')
            .next()
            .unwrap()
            .split_whitespace()
            .nth(3)
            .unwrap()
            .parse()
            .unwrap()
    }

    /// Whether this host has pasta (a test that needs it says so and skips
    /// where it hasn't; with `SHIPYARD_TEST_TOOLS=required`, where it's
    /// installed, it fails instead: a skip would hide that it never ran).
    fn pasta_here() -> bool {
        let here = shipyard_network::pasta::find(std::env::var_os("PATH").as_deref()).is_some();
        if !here {
            assert!(
                std::env::var("SHIPYARD_TEST_TOOLS").as_deref() != Ok("required"),
                "pasta is required here (SHIPYARD_TEST_TOOLS), but isn't on the PATH"
            );
            eprintln!("(no pasta here: skipped)");
        }
        here
    }

    /// How many sockets process `pid` has open (its descriptors').
    fn sockets(pid: i32) -> usize {
        std::fs::read_dir(format!("/proc/{pid}/fd"))
            .unwrap()
            .flatten()
            .filter(|fd| {
                std::fs::read_link(fd.path())
                    .is_ok_and(|target| target.to_string_lossy().starts_with("socket:"))
            })
            .count()
    }

    /// The processes whose command line has `word`.
    fn processes_with(word: &str) -> Vec<String> {
        std::fs::read_dir("/proc")
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let line = std::fs::read(entry.path().join("cmdline")).ok()?;
                let line = String::from_utf8_lossy(&line).replace('\0', " ");
                line.contains(word)
                    .then(|| format!("{}: {line}", entry.file_name().to_string_lossy()))
            })
            .collect()
    }

    /// Whether `path` is locked (`flock`) by anyone.
    fn locked(path: &Path) -> bool {
        let Ok(file) = std::fs::File::open(path) else {
            return false;
        };
        matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock))
    }

    /// Waits for the image store's writable areas to be let go (none held:
    /// a container's own is kept, free, until it's removed; a run's is
    /// gone), saying (if they never are) what `__cleanup` said and who
    /// still holds each.
    fn areas_let_go(stores: &Stores, id: &ContainerId) {
        let deadline = Instant::now() + Duration::from_secs(30);
        let held = || {
            stores.areas().iter().any(|area| {
                area.starts_with("run-")
                    || locked(&stores.scratch.store().join("containers").join(area))
            })
        };
        while held() {
            if Instant::now() > deadline {
                let holders: Vec<_> = stores
                    .areas()
                    .iter()
                    .map(|area| {
                        let path = stores.scratch.store().join("containers").join(area);
                        (area.clone(), locked(&path), holders(&path))
                    })
                    .collect();
                panic!(
                    "never: the areas let go: {holders:?}; cleanup.log: {:?}",
                    stores.cleanup_log(id)
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The processes with a descriptor open on `path`, and their command.
    fn holders(path: &Path) -> Vec<String> {
        let mut found = Vec::new();
        for process in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
            let fds = process.path().join("fd");
            for fd in std::fs::read_dir(&fds).into_iter().flatten().flatten() {
                if std::fs::read_link(fd.path()).is_ok_and(|target| target == path) {
                    let command = std::fs::read_to_string(process.path().join("cmdline"))
                        .unwrap_or_default()
                        .replace('\0', " ");
                    found.push(format!(
                        "{}: {command}",
                        process.file_name().to_string_lossy()
                    ));
                }
            }
        }
        found
    }

    /// Waits for `found` (up to 30 s), naming `what` if it never comes.
    fn eventually(what: &str, mut found: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !found() {
            assert!(Instant::now() < deadline, "never: {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A free TCP port on the host, for `--publish`.
    fn free_port() -> u16 {
        std::net::TcpListener::bind("0.0.0.0:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// Whether the host's TCP `port` can be bound (nothing holds it).
    fn bindable(port: u16) -> bool {
        std::net::TcpListener::bind(("0.0.0.0", port)).is_ok()
    }

    /// The core: once the "CLI" has gone, the container runs on and holds
    /// its writable area, its volume, and (root) its lease and published
    /// port; killed, everything is let go and its end recorded.
    fn a_detached_container_outlives_its_caller_and_is_cleaned_up() {
        let stores = Stores::new();
        let port = free_port();
        let publish = format!("{port}:80");
        let mut args = vec!["--volume", "data:/data"];
        if is_root() {
            args.extend(["--publish", publish.as_str()]);
        }
        args.extend([
            stores.image.as_str(),
            "sh",
            "-c",
            "echo hello; echo kept > /data/f; sleep 300",
        ]);
        let started = Instant::now();
        let output = stores.launch(&args);
        // H7: back to the caller quickly (the pull is local).
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{:?}",
            started.elapsed()
        );
        assert!(output.status.success(), "{output:?}");
        let id = Stores::id(&output);
        // The caller has gone; the container runs.
        assert!(
            matches!(stores.state(&id), ContainerState::Running { .. }),
            "{:?}",
            stores.state(&id)
        );
        assert!(stores.record(&id).started.is_some());
        // Its monitor isn't deaf to SIGTERM: the caller held its signals off
        // while it created it, and a mask survives fork and exec.
        let monitor = parent(
            runtime::state(&stores.runtime_root(), id.as_str())
                .unwrap()
                .pid
                .unwrap(),
        );
        assert_eq!(
            blocked_signals(monitor),
            "0000000000000000",
            "the monitor's mask"
        );
        let logs = containers::testing::logs_dir(&stores.engine(), &id).unwrap();
        eventually("hello in the logs", || {
            names(&logs).iter().any(|start| {
                std::fs::read_to_string(logs.join(start).join("container.log"))
                    .is_ok_and(|log| log.contains("hello"))
            })
        });
        let areas = stores.areas();
        assert_eq!(areas.len(), 1, "{areas:?}");
        let area = stores.scratch.store().join("containers").join(&areas[0]);
        assert!(locked(&area), "the area is held");
        let volume_lock = stores.scratch.store().join("volumes/.locks/data");
        assert!(locked(&volume_lock), "the volume is held");
        let mut run_dir = None;
        if is_root() {
            let leases = stores.leases();
            assert_eq!(leases.len(), 1, "{leases:?}");
            assert!(locked(&leases[0]), "the lease is held");
            assert!(!bindable(port), "the published port is held");
            let dir = stores.run_dir(&leases[0]);
            assert!(locked(&dir.join("lock")), "the run directory is held");
            run_dir = Some(dir);
        }
        // Killed: everything goes, and its end is recorded.
        runtime::kill(
            &stores.runtime_root(),
            id.as_str(),
            shipyard_runtime::process::Signal::KILL,
        )
        .unwrap();
        let killed = Instant::now();
        eventually("the record's exit", || stores.record(&id).exit.is_some());
        // Promptly: `__cleanup` waits only while its monitor is there.
        assert!(
            killed.elapsed() < Duration::from_secs(5),
            "{:?}",
            killed.elapsed()
        );
        let record = stores.record(&id);
        assert_eq!(record.exit.unwrap().code, Some(137));
        assert!(record.finished.is_some());
        eventually("the runtime state gone", || {
            runtime::status(&stores.runtime_root(), id.as_str())
                .unwrap()
                .is_none()
        });
        assert!(
            matches!(
                stores.state(&id),
                ContainerState::Exited {
                    code: Some(137),
                    ..
                }
            ),
            "{:?}",
            stores.state(&id)
        );
        assert!(!locked(&area), "the area is let go");
        assert!(
            area.is_dir(),
            "and kept, the container's, until it's removed"
        );
        assert!(!locked(&volume_lock), "the volume is free");
        if is_root() {
            assert!(stores.leases().is_empty(), "the lease is gone");
            assert!(bindable(port), "the port is free");
        }
        if let Some(dir) = run_dir {
            assert!(!dir.exists(), "the run directory is removed");
        }
        assert_eq!(stores.cleanup_log(&id), "", "nothing left for later");
        // Removed: its area, its image's pin and its record go.
        shipyard_engine::run::remove(&stores.engine(), id.as_str(), false).unwrap();
        assert!(!area.exists(), "the area goes with the container");
        assert!(containers::testing::find(&stores.engine(), id.as_str()).is_err());
        assert!(
            !std::fs::read_to_string(stores.scratch.store().join("index.json"))
                .unwrap_or_default()
                .contains(id.as_str()),
            "its pin gone"
        );
    }

    /// A container whose start fails (its init killed between create and
    /// start) is `Created`, never started, with no exit, and holds nothing.
    fn a_failed_start_leaves_a_created_container_holding_nothing() {
        let stores = Stores::new();
        let output = stores.launch(&["--kill-init", stores.image.as_str(), "sleep", "300"]);
        assert!(!output.status.success(), "{output:?}");
        let records = containers::list(&stores.engine()).unwrap();
        assert_eq!(records.len(), 1, "the record is kept");
        let id = records[0].id.clone();
        eventually("the runtime state gone", || {
            runtime::status(&stores.runtime_root(), id.as_str())
                .unwrap()
                .is_none()
        });
        let record = stores.record(&id);
        assert_eq!(record.started, None);
        assert_eq!(record.exit, None, "a forced end isn't an exit");
        assert_eq!(stores.state(&id), ContainerState::Created);
        areas_let_go(&stores, &id);
        if is_root() {
            eventually("the lease gone", || stores.leases().is_empty());
        }
    }

    /// A container that ran, and whose next start fails: its record keeps
    /// what its last start left (its end, its code), so it reads as exited,
    /// never as never started, and a boot still brings it back. Whatever
    /// order the failed start's clean-up and its caller's undo come in.
    fn a_failed_start_keeps_what_its_last_start_left() {
        let stores = Stores::new();
        let id = Stores::id(&stores.launch(&[stores.image.as_str(), "sh", "-c", "exit 3"]));
        // Its end recorded: its clean-up done (the runtime says it ended
        // before the record does).
        eventually("its first end, recorded", || {
            matches!(
                stores.state(&id),
                ContainerState::Exited { code: Some(3), .. }
            ) && stores.running_file(&id).try_lock().is_ok()
        });
        let first = stores.record(&id);
        assert!(first.exit.is_some(), "{first:?}");
        let failed = stores
            .container_command("start-kill-init", id.as_str())
            .output()
            .unwrap();
        assert!(!failed.status.success(), "{failed:?}");
        eventually("the runtime state gone", || {
            runtime::status(&stores.runtime_root(), id.as_str())
                .unwrap()
                .is_none()
        });
        // Its clean-up has run too (it lets go of `running` last, after its
        // record and its event).
        eventually("the clean-up done", || {
            stores.running_file(&id).try_lock().is_ok()
        });
        // One `die`, its first start's: the start that never ran has none,
        // though its record has the first start's end again (F80).
        let actions = stores.actions(id.as_str());
        assert_eq!(
            actions.iter().filter(|action| *action == "die").count(),
            1,
            "{actions:?}"
        );
        let record = stores.record(&id);
        assert_eq!(record.started, first.started, "its last start");
        assert_eq!(record.exit, first.exit, "its last end");
        assert_eq!(record.boot_id, first.boot_id);
        assert!(
            matches!(
                stores.state(&id),
                ContainerState::Exited { code: Some(3), .. }
            ),
            "{:?}",
            stores.state(&id)
        );
        areas_let_go(&stores, &id);
    }

    /// A start that fails with the container's init alive (its gate
    /// broken): `detached` ends it, and its clean-up lets go of everything;
    /// the record stays, never started.
    fn a_start_that_fails_with_its_init_alive_ends_it() {
        let stores = Stores::new();
        let output = stores.launch(&["--break-start", stores.image.as_str(), "sleep", "300"]);
        assert!(!output.status.success(), "{output:?}");
        let records = containers::list(&stores.engine()).unwrap();
        assert_eq!(records.len(), 1, "the record is kept");
        let id = records[0].id.clone();
        eventually("the runtime state gone", || {
            runtime::status(&stores.runtime_root(), id.as_str())
                .unwrap()
                .is_none()
        });
        assert_eq!(stores.state(&id), ContainerState::Created);
        // Its clean-up (done when `running` is free) recorded no start, no
        // end and no exit: a forced end of a start that never ran isn't one
        // (F64). (The runtime state went with `detached`'s own forced
        // delete, before the clean-up ran.)
        eventually("the clean-up done", || {
            stores.running_file(&id).try_lock().is_ok()
        });
        let record = stores.record(&id);
        assert_eq!(
            (record.started, record.finished, record.exit),
            (None, None, None)
        );
        areas_let_go(&stores, &id);
        if is_root() {
            eventually("the lease gone", || stores.leases().is_empty());
        }
    }

    /// A start that fails before the container is made (its published port
    /// taken): the container stays, made, never started (Docker's), and
    /// nothing is held.
    fn a_failure_before_create_leaves_a_created_record() {
        if !is_root() {
            return;
        }
        let stores = Stores::new();
        let taken = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        let publish = format!("{port}:80");
        let output = stores.launch(&["--publish", &publish, stores.image.as_str(), "true"]);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("error=Network") && stdout.contains("already in use"),
            "{output:?}"
        );
        let records = containers::list(&stores.engine()).unwrap();
        assert_eq!(records.len(), 1, "the record is kept");
        assert_eq!(stores.state(&records[0].id), ContainerState::Created);
        areas_let_go(&stores, &records[0].id);
        assert!(stores.leases().is_empty(), "no lease");
    }

    /// A command that doesn't exist: the start succeeds (it only opens the
    /// gate), the container ends 127, and everything is let go.
    fn a_command_that_doesnt_exist_ends_127_holding_nothing() {
        let stores = Stores::new();
        let output = stores.launch(&[stores.image.as_str(), "no-such-command"]);
        assert!(output.status.success(), "{output:?}");
        let id = Stores::id(&output);
        eventually("the record's exit", || stores.record(&id).exit.is_some());
        assert_eq!(stores.record(&id).exit.unwrap().code, Some(127));
        areas_let_go(&stores, &id);
        if is_root() {
            eventually("the lease gone", || stores.leases().is_empty());
        }
    }

    /// On a network, a sysctl of an interface it doesn't get is refused
    /// before anything is made; one of an interface it gets (`eth0`) is set
    /// once it's made, and `all`'s by its init, as any other.
    fn interface_sysctls_need_their_interface() {
        if !is_root() {
            return;
        }
        let stores = Stores::new();
        let output = stores.launch(&[
            "--sysctl",
            "net.ipv4.conf.eth3.rp_filter=2",
            stores.image.as_str(),
            "true",
        ]);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("error=InvalidRequest"), "{output:?}");
        assert!(containers::list(&stores.engine()).unwrap().is_empty());
        assert!(stores.areas().is_empty());
        assert!(stores.leases().is_empty());
        // Its own interface's, and all of them: set (its exit says what
        // eth0's is).
        let output = stores.launch(&[
            "--sysctl",
            "net.ipv4.conf.eth0.rp_filter=2",
            "--sysctl",
            "net.ipv4.conf.all.log_martians=1",
            stores.image.as_str(),
            "sh",
            "-c",
            "read value < /proc/sys/net/ipv4/conf/eth0/rp_filter; exit $value",
        ]);
        let id = Stores::id(&output);
        eventually("the record's exit", || stores.record(&id).exit.is_some());
        assert_eq!(stores.record(&id).exit.unwrap().code, Some(2));
    }

    /// A Ctrl-C while the record is made and the network is being set up
    /// (waiting for the network's lock, here) ends the call cleanly: no
    /// record, nothing held; the caller then dies of it.
    fn a_ctrl_c_before_create_undoes_everything() {
        if !is_root() {
            return;
        }
        let stores = Stores::new();
        // The default network made, by a first container.
        let first = Stores::id(&stores.launch(&[stores.image.as_str(), "true"]));
        eventually("the first one's exit", || {
            stores.record(&first).exit.is_some()
        });
        let networks =
            shipyard_network::store::Networks::open(&stores.scratch.0.join("networks")).unwrap();
        let held = networks.lock("bridge").unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["launch", stores.image.as_str(), "sleep", "300"])
            .stdout(std::process::Stdio::piped());
        for (key, value) in stores.env() {
            command.env(key, value);
        }
        let child = command.spawn().unwrap();
        // Waiting for the lock, with its record made.
        eventually("the second record", || {
            containers::list(&stores.engine()).unwrap().len() == 2
        });
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap()),
            nix::sys::signal::Signal::SIGINT,
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        drop(held);
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.signal(), Some(libc::SIGINT), "{output:?}");
        let records = containers::list(&stores.engine()).unwrap();
        assert_eq!(records.len(), 1, "only the first: {records:?}");
        assert_eq!(
            stores.areas(),
            [format!("ctr-{}", first.as_str())],
            "only the first's"
        );
        assert!(stores.leases().is_empty(), "{:?}", stores.leases());
        // Said to go only if it was said to be made (an undone create
        // before then is never said).
        let events = stores.events();
        for (at, gone) in events.iter().enumerate() {
            if gone.action == "destroy" {
                assert!(
                    events[..at]
                        .iter()
                        .any(|made| made.id == gone.id && made.action == "create"),
                    "{events:?}"
                );
            }
        }
    }

    /// A Ctrl-C between create and start waits for the start: the container
    /// runs, detached; the caller then dies of it.
    fn a_ctrl_c_between_create_and_start_waits() {
        let stores = Stores::new();
        let output = stores.launch(&["--interrupt", stores.image.as_str(), "sleep", "300"]);
        assert_eq!(output.status.signal(), Some(libc::SIGINT), "{output:?}");
        let records = containers::list(&stores.engine()).unwrap();
        assert_eq!(records.len(), 1);
        let id = records[0].id.clone();
        assert!(
            matches!(stores.state(&id), ContainerState::Running { .. }),
            "{:?}",
            stores.state(&id)
        );
        runtime::kill(
            &stores.runtime_root(),
            id.as_str(),
            shipyard_runtime::process::Signal::KILL,
        )
        .unwrap();
        eventually("the record's exit", || stores.record(&id).exit.is_some());
    }

    /// A monitor killed outright runs no `__cleanup`: what it held is free
    /// (its locks died with it) and the next run's sweeps reclaim it; its
    /// container's own area is kept (the container is still there), and
    /// removing the container settles what's left (its runtime state).
    fn a_monitor_killed_leaves_what_the_next_run_sweeps() {
        let stores = Stores::new();
        let output = stores.launch(&[stores.image.as_str(), "sleep", "300"]);
        let id = Stores::id(&output);
        let init = runtime::state(&stores.runtime_root(), id.as_str())
            .unwrap()
            .pid
            .unwrap();
        let monitor = parent(init);
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(monitor),
            nix::sys::signal::Signal::SIGKILL,
        )
        .unwrap();
        let area = stores.areas()[0].clone();
        let area_path = stores.scratch.store().join("containers").join(&area);
        eventually("the area's lock gone with the monitor", || {
            !locked(&area_path)
        });
        // The next run sweeps the first's leftovers, but its container's
        // own area.
        let next = stores.launch(&[stores.image.as_str(), "sleep", "300"]);
        let next = Stores::id(&next);
        assert!(stores.areas().contains(&area), "the container's area kept");
        if is_root() {
            assert_eq!(stores.leases().len(), 1, "only the live run's lease");
        }
        assert!(stores.record(&id).exit.is_none(), "no clean-up ran");
        runtime::kill(
            &stores.runtime_root(),
            next.as_str(),
            shipyard_runtime::process::Signal::KILL,
        )
        .unwrap();
        eventually("the next one cleaned up", || {
            stores.record(&next).exit.is_some()
        });
        // Its runtime state, left: removing the container settles it.
        shipyard_engine::run::remove(&stores.engine(), id.as_str(), false).unwrap();
        assert!(
            runtime::status(&stores.runtime_root(), id.as_str())
                .unwrap()
                .is_none()
        );
        assert!(!stores.areas().contains(&area), "the area goes with it");
    }

    /// A container whose monitor was killed (nothing recorded its end): the
    /// next command that makes a container settles it (stage 9, step 9.5a).
    /// Its end gets a time (its code stays unknown), its runtime state goes
    /// (and its cgroup with it), and it's said.
    fn a_killed_monitors_container_is_settled_by_the_next_create() {
        let stores = Stores::new();
        let id = Stores::id(&stores.launch(&[
            "--network",
            "none",
            stores.image.as_str(),
            "sleep",
            "300",
        ]));
        let started = stores.record(&id).started.unwrap();
        stores.kill_monitor(&id);
        assert!(
            matches!(
                stores.state(&id),
                ContainerState::Exited {
                    code: None,
                    finished: None,
                    ..
                }
            ),
            "{:?}",
            stores.state(&id)
        );
        let (engine, seen) = stores.engine_collecting();
        let other =
            shipyard_engine::run::create(&engine, stores.request(&["true"]), false).unwrap();
        let record = stores.record(&id);
        assert!(record.finished.is_some_and(|finished| finished >= started));
        assert!(record.exit.is_none(), "how it ended isn't known");
        assert!(
            runtime::status(&stores.runtime_root(), id.as_str())
                .unwrap()
                .is_none(),
            "its runtime state settled"
        );
        let said: Vec<_> = seen
            .warnings()
            .into_iter()
            .filter(|warning| warning.kind == WarningKind::Recovered)
            .collect();
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].message.contains(record.name.as_str()), "{said:?}");
        // Settled once: the next pass has nothing to say.
        let (engine, seen) = stores.engine_collecting();
        shipyard_engine::run::remove(&engine, other.id.as_str(), false).unwrap();
        assert!(
            seen.warnings()
                .iter()
                .all(|w| w.kind != WarningKind::Recovered)
        );
        shipyard_engine::run::remove(&engine, id.as_str(), false).unwrap();
    }

    /// A `--rm` container whose monitor was killed: removed by the next
    /// command that makes a container, its writable layer with it, as its
    /// clean-up would have.
    fn a_crashed_rm_container_is_removed() {
        let stores = Stores::new();
        let id = Stores::id(&stores.launch(&[
            "--network",
            "none",
            "--rm",
            stores.image.as_str(),
            "sleep",
            "300",
        ]));
        stores.kill_monitor(&id);
        assert!(stores.areas().iter().any(|area| area.contains(id.as_str())));
        let (engine, seen) = stores.engine_collecting();
        let other =
            shipyard_engine::run::create(&engine, stores.request(&["true"]), false).unwrap();
        assert_eq!(
            containers::testing::find(&stores.engine(), id.as_str())
                .unwrap_err()
                .kind(),
            ErrorKind::NotFound
        );
        assert!(!stores.areas().iter().any(|area| area.contains(id.as_str())));
        assert!(
            seen.warnings()
                .iter()
                .any(|w| w.kind == WarningKind::Recovered && w.message.contains("--rm")),
            "{:?}",
            seen.warnings()
        );
        shipyard_engine::run::remove(&engine, other.id.as_str(), false).unwrap();
    }

    /// A container whose record can't be read (a newer Shipyard's, after a
    /// downgrade) blocks every new container (its name and serial may be
    /// taken): `rm` by its full ID says how out, and `rm -f` gets out,
    /// what's left of it killed, its area, pin and directory gone.
    fn an_unreadable_container_is_removed_by_its_id_with_force() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id = Stores::id(&stores.launch(&[
            "--network",
            "none",
            stores.image.as_str(),
            "sleep",
            "300",
        ]));
        let record = containers::testing::record_dir(&engine, &id)
            .unwrap()
            .join("record.json");
        std::fs::write(&record, r#"{"version": 4}"#).unwrap();
        // What it blocks (the control: why it must be removable).
        let blocked =
            shipyard_engine::run::create(&engine, stores.request(&["true"]), false).unwrap_err();
        assert_eq!(blocked.kind(), ErrorKind::Storage, "{blocked:#}");
        assert!(format!("{blocked:#}").contains("rm -f"), "{blocked:#}");
        // Not without `force`, and not by a prefix: only its full ID names it.
        let refused = shipyard_engine::run::remove(&engine, id.as_str(), false).unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::Storage, "{refused:#}");
        assert!(
            format!("{refused:#}").contains(&format!("rm -f {id}")),
            "{refused:#}"
        );
        assert_eq!(
            shipyard_engine::run::remove(&engine, id.short(), true)
                .unwrap_err()
                .kind(),
            ErrorKind::NotFound
        );
        assert!(record.exists(), "nothing removed yet");
        assert!(
            runtime::status(&stores.runtime_root(), id.as_str())
                .unwrap()
                .is_some()
        );
        // Forced: all its ID names goes.
        assert_eq!(
            shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap(),
            id
        );
        assert!(!record.parent().unwrap().exists(), "its directory gone");
        assert!(
            runtime::status(&stores.runtime_root(), id.as_str())
                .unwrap()
                .is_none()
        );
        assert!(!stores.areas().iter().any(|area| area.contains(id.as_str())));
        assert!(
            !std::fs::read_to_string(stores.scratch.store().join("index.json"))
                .unwrap_or_default()
                .contains(id.as_str()),
            "its pin gone"
        );
        let next = shipyard_engine::run::create(&engine, stores.request(&["true"]), false).unwrap();
        shipyard_engine::run::remove(&engine, next.id.as_str(), false).unwrap();
    }

    /// `start` of a `--rm` container whose monitor was killed: the pass
    /// removes it (it settles the command's own container too), so it's
    /// gone, not started again.
    fn a_crashed_rm_container_isnt_started_again() {
        let stores = Stores::new();
        let id = Stores::id(&stores.launch(&[
            "--network",
            "none",
            "--rm",
            stores.image.as_str(),
            "sleep",
            "300",
        ]));
        stores.kill_monitor(&id);
        let engine = stores.engine();
        let error = shipyard_engine::run::start(&engine, id.as_str()).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::NotFound, "{error:#}");
        assert_eq!(
            containers::testing::find(&engine, id.as_str())
                .unwrap_err()
                .kind(),
            ErrorKind::NotFound
        );
    }

    /// A start of an earlier boot (its record's boot ID another, its runtime
    /// state gone, as a reboot leaves it): settled as ended when the host
    /// booted.
    fn a_reboot_is_told_by_the_boot_id() {
        let stores = Stores::new();
        let id = Stores::id(&stores.launch(&[
            "--network",
            "none",
            stores.image.as_str(),
            "sleep",
            "300",
        ]));
        assert!(
            stores.record(&id).boot_id.is_some(),
            "a start records its boot"
        );
        stores.kill_monitor(&id);
        // What a reboot leaves: no runtime state, a start of another boot.
        runtime::delete(&stores.runtime_root(), id.as_str(), true).unwrap();
        let btime: u64 = std::fs::read_to_string("/proc/stat")
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("btime "))
            .unwrap()
            .parse()
            .unwrap();
        let booted = SystemTime::UNIX_EPOCH + Duration::from_secs(btime);
        // Started before this boot, in another.
        containers::testing::update(&stores.engine(), &id, |record| {
            record.boot_id = Some("an-earlier-boot".to_owned());
            record.started = Some(booted - Duration::from_secs(60));
        })
        .unwrap();
        let other =
            shipyard_engine::run::create(&stores.engine(), stores.request(&["true"]), false)
                .unwrap();
        assert_eq!(stores.record(&id).finished, Some(booted));
        let engine = stores.engine();
        shipyard_engine::run::remove(&engine, other.id.as_str(), false).unwrap();
        shipyard_engine::run::remove(&engine, id.as_str(), false).unwrap();
    }

    /// A container another command works on (its operation lock held), or
    /// whose last start's holders haven't all let go (its `running` file
    /// held), is left alone by the pass, which never waits: the next pass
    /// settles it.
    fn recovery_leaves_a_busy_container_alone() {
        let stores = Stores::new();
        let id = Stores::id(&stores.launch(&[
            "--network",
            "none",
            stores.image.as_str(),
            "sleep",
            "300",
        ]));
        stores.kill_monitor(&id);
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(
                containers::testing::record_dir(&stores.engine(), &id)
                    .unwrap()
                    .join("op.lock"),
            )
            .unwrap();
        lock.lock().unwrap();
        let engine = stores.engine();
        let began = Instant::now();
        let other =
            shipyard_engine::run::create(&engine, stores.request(&["true"]), false).unwrap();
        assert!(began.elapsed() < Duration::from_secs(5), "it waited");
        assert!(stores.record(&id).finished.is_none(), "settled while busy");
        drop(lock);
        // Its `running` file held (its clean-up still at work): left too.
        let running = stores.running_file(&id);
        running.lock().unwrap();
        let next = shipyard_engine::run::create(&engine, stores.request(&["true"]), false).unwrap();
        assert!(
            stores.record(&id).finished.is_none(),
            "settled while its clean-up held it"
        );
        drop(running);
        shipyard_engine::run::remove(&engine, other.id.as_str(), false).unwrap();
        assert!(
            stores.record(&id).finished.is_some(),
            "the next pass settles it"
        );
        shipyard_engine::run::remove(&engine, next.id.as_str(), false).unwrap();
        shipyard_engine::run::remove(&engine, id.as_str(), false).unwrap();
    }

    /// A start killed with its container made and not started (its monitor
    /// waiting at the gate, holding what it was given): the next pass
    /// deletes it, its monitor with it, and the container is as made.
    fn a_dead_start_is_settled() {
        let stores = Stores::new();
        let killed = stores.launch(&[
            "--network",
            "none",
            "--abort-between",
            stores.image.as_str(),
            "sleep",
            "300",
        ]);
        assert!(!killed.status.success());
        let engine = stores.engine();
        let made = containers::list(&engine).unwrap();
        assert_eq!(made.len(), 1);
        let id = made[0].id.clone();
        assert!(matches!(
            runtime::status(&stores.runtime_root(), id.as_str()).unwrap(),
            Some(runtime::Status::Created | runtime::Status::Creating)
        ));
        let other =
            shipyard_engine::run::create(&engine, stores.request(&["true"]), false).unwrap();
        assert!(
            runtime::status(&stores.runtime_root(), id.as_str())
                .unwrap()
                .is_none()
        );
        assert_eq!(stores.state(&id), ContainerState::Created);
        shipyard_engine::run::remove(&engine, other.id.as_str(), false).unwrap();
        shipyard_engine::run::remove(&engine, id.as_str(), false).unwrap();
    }

    /// `stop` and `kill` record that the user stopped the container (before
    /// anything is sent: a restart policy leaves it so); a start forgets it.
    fn a_users_stop_is_recorded_and_a_start_forgets_it() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id = Stores::id(&stores.launch(&[
            "--network",
            "none",
            stores.image.as_str(),
            "sleep",
            "300",
        ]));
        assert!(!stores.record(&id).stopped_by_user);
        let fast = shipyard_engine::run::StopOptions {
            timeout: Some(Duration::ZERO),
            ..Default::default()
        };
        shipyard_engine::run::stop(&engine, id.as_str(), fast).unwrap();
        assert!(stores.record(&id).stopped_by_user, "stop");
        shipyard_engine::run::start(&engine, id.as_str()).unwrap();
        assert!(!stores.record(&id).stopped_by_user, "start");
        // A signal that isn't a stop (SIGWINCH: ignored) isn't one.
        shipyard_engine::run::kill(
            &engine,
            id.as_str(),
            Signal::from_raw(libc::SIGWINCH).unwrap(),
        )
        .unwrap();
        assert!(!stores.record(&id).stopped_by_user, "kill -s WINCH");
        shipyard_engine::run::kill(&engine, id.as_str(), Signal::KILL).unwrap();
        assert!(stores.record(&id).stopped_by_user, "kill");
        eventually("the kill's clean-up", || stores.record(&id).exit.is_some());
        shipyard_engine::run::restart(&engine, id.as_str(), fast).unwrap();
        assert!(
            !stores.record(&id).stopped_by_user,
            "restart starts it again"
        );
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// `on-failure:2`: a container that fails is started again, twice, then
    /// left ended (its code its last); each restart a start of its own.
    fn on_failure_restarts_up_to_its_max() {
        let stores = Stores::new();
        let id = stores.launch_restarting("on-failure:2", "exit 3");
        eventually("two restarts, then the end", || {
            let record = stores.record(&id);
            record.restart_count == 2
                && record.next_start.is_none()
                && matches!(
                    stores.state(&id),
                    ContainerState::Exited { code: Some(3), .. }
                )
        });
        std::thread::sleep(Duration::from_secs(1));
        assert_eq!(stores.record(&id).restart_count, 2, "a third restart");
        assert_eq!(stores.starts(&id), 3);
        shipyard_engine::run::remove(&stores.engine(), id.as_str(), false).unwrap();
    }

    /// `on-failure` leaves a clean end (0) as it is.
    fn on_failure_leaves_a_clean_exit() {
        let stores = Stores::new();
        let id = stores.launch_restarting("on-failure", "exit 0");
        eventually("its end", || {
            matches!(
                stores.state(&id),
                ContainerState::Exited { code: Some(0), .. }
            )
        });
        std::thread::sleep(Duration::from_secs(1));
        assert_eq!(stores.record(&id).restart_count, 0);
        assert_eq!(stores.starts(&id), 1);
        shipyard_engine::run::remove(&stores.engine(), id.as_str(), false).unwrap();
    }

    /// `always` starts a container again however it ends (0 too), until
    /// its user stops it: then it's left ended.
    fn always_restarts_until_its_user_stops_it() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id = stores.launch_restarting("always", "sleep 0.3");
        eventually("two restarts", || stores.record(&id).restart_count >= 2);
        let fast = shipyard_engine::run::StopOptions {
            timeout: Some(Duration::ZERO),
            ..Default::default()
        };
        shipyard_engine::run::stop(&engine, id.as_str(), fast).unwrap();
        let count = stores.record(&id).restart_count;
        std::thread::sleep(Duration::from_secs(2));
        let record = stores.record(&id);
        assert!(record.stopped_by_user);
        assert_eq!(record.restart_count, count, "restarted after its stop");
        assert!(matches!(stores.state(&id), ContainerState::Exited { .. }));
        shipyard_engine::run::remove(&engine, id.as_str(), false).unwrap();
    }

    /// `stop` of a container waiting to restart: it's exited at once, and
    /// what waited gives up.
    fn a_stop_while_restarting_ends_the_restarts() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id = stores.launch_restarting("always", "sleep 1; exit 1");
        stores.slow_restart(&id);
        let waiter = Stores::restart_waiter(&id);
        // A recovery pass meanwhile leaves a restart that waits alone.
        let other =
            shipyard_engine::run::create(&engine, stores.request(&["true"]), false).unwrap();
        assert!(
            matches!(stores.state(&id), ContainerState::Restarting { .. }),
            "{:?}",
            stores.state(&id)
        );
        shipyard_engine::run::remove(&engine, other.id.as_str(), false).unwrap();
        shipyard_engine::run::stop(
            &engine,
            id.as_str(),
            shipyard_engine::run::StopOptions::default(),
        )
        .unwrap();
        assert!(matches!(
            stores.state(&id),
            ContainerState::Exited { code: Some(1), .. }
        ));
        Stores::waiter_gone(waiter);
        assert!(
            runtime::status(&stores.runtime_root(), id.as_str())
                .unwrap()
                .is_none(),
            "started again"
        );
        shipyard_engine::run::remove(&engine, id.as_str(), false).unwrap();
    }

    /// `start` of a container waiting to restart starts it now (its user's
    /// start: its count afresh); what waited steps aside at once, never
    /// starting it a second time.
    fn a_start_while_restarting_takes_over() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id = stores.launch_restarting("always", "sleep 1; exit 1");
        stores.slow_restart(&id);
        let waiter = Stores::restart_waiter(&id);
        shipyard_engine::run::start(&engine, id.as_str()).unwrap();
        let started = Instant::now();
        Stores::waiter_gone(waiter);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the waiter waited on"
        );
        assert_eq!(stores.record(&id).restart_count, 0, "a user's start");
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// After a reboot (simulated: every monitor killed, the runtime state
    /// gone, the records' starts of an earlier boot), `boot` settles what's
    /// left, then starts what the policies say: `always` even after its
    /// user stopped it, `unless-stopped` unless they did, `on-failure` after
    /// an end nothing recorded (a failure) but not after a clean one, `no`
    /// never. A second `boot` starts nothing more.
    fn boot_starts_what_the_policies_say() {
        let stores = Stores::new();
        let engine = stores.engine();
        let fast = shipyard_engine::run::StopOptions {
            timeout: Some(Duration::ZERO),
            ..Default::default()
        };
        let always_stopped = stores.launch_restarting("always", "sleep 300");
        shipyard_engine::run::stop(&engine, always_stopped.as_str(), fast).unwrap();
        let unless_up = stores.launch_restarting("unless-stopped", "sleep 300");
        let unless_stopped = stores.launch_restarting("unless-stopped", "sleep 300");
        shipyard_engine::run::stop(&engine, unless_stopped.as_str(), fast).unwrap();
        let failure_clean = stores.launch_restarting("on-failure", "exit 0");
        eventually("its clean end", || {
            matches!(
                stores.state(&failure_clean),
                ContainerState::Exited { code: Some(0), .. }
            )
        });
        let failure_up = stores.launch_restarting("on-failure", "sleep 300");
        let never = stores.launch_restarting("no", "sleep 300");
        // The reboot.
        for up in [&unless_up, &failure_up, &never] {
            stores.kill_monitor(up);
        }
        let all = [
            &always_stopped,
            &unless_up,
            &unless_stopped,
            &failure_clean,
            &failure_up,
            &never,
        ];
        for id in all {
            runtime::delete(&stores.runtime_root(), id.as_str(), true).ok();
            containers::testing::update(&engine, id, |record| {
                record.boot_id = Some("an-earlier-boot".to_owned());
            })
            .unwrap();
        }
        let booted = shipyard_engine::run::boot(&engine).unwrap();
        assert!(booted.failed.is_empty(), "{:?}", booted.failed);
        let mut started: Vec<String> = booted.started.iter().map(ToString::to_string).collect();
        started.sort();
        let mut expected: Vec<String> = [&always_stopped, &unless_up, &failure_up]
            .iter()
            .map(|id| stores.record(id).name.to_string())
            .collect();
        expected.sort();
        assert_eq!(started, expected);
        for id in [&always_stopped, &unless_up, &failure_up] {
            assert!(
                matches!(stores.state(id), ContainerState::Running { .. }),
                "{:?}",
                stores.state(id)
            );
        }
        for id in [&unless_stopped, &failure_clean, &never] {
            assert!(
                matches!(stores.state(id), ContainerState::Exited { .. }),
                "{:?}",
                stores.state(id)
            );
        }
        // What the reboot left, settled first: the end given a time.
        assert!(stores.record(&never).finished.is_some());
        let again = shipyard_engine::run::boot(&engine).unwrap();
        assert!(again.started.is_empty(), "{:?}", again.started);
        for id in all {
            shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
        }
        // A reboot where nothing comes back: `boot` settles all the same.
        let lone = stores.launch_restarting("no", "sleep 300");
        stores.kill_monitor(&lone);
        runtime::delete(&stores.runtime_root(), lone.as_str(), true).ok();
        let booted = shipyard_engine::run::boot(&engine).unwrap();
        assert!(booted.started.is_empty(), "{:?}", booted.started);
        assert!(stores.record(&lone).finished.is_some(), "not settled");
        shipyard_engine::run::remove(&engine, lone.as_str(), true).unwrap();
    }

    /// The host-stopping marker (`.host-stopping` in the runtime root,
    /// which as root is the host's `/run/shipyard`, shared with every other
    /// test and Shipyard): removed when this goes, whatever the test did.
    struct HostStopping(PathBuf);

    impl HostStopping {
        fn at(stores: &Stores) -> Self {
            Self(stores.runtime_root().join(".host-stopping"))
        }
    }

    impl Drop for HostStopping {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// `system stop` (the host's shutdown): each container stopped with
    /// its own stop signal (its trap runs), not as its user's stop, and no
    /// restart policy fires until `boot`, which brings it back.
    fn the_host_stop_is_graceful_and_not_a_users() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id = stores.launch_restarting(
            "unless-stopped",
            "trap 'exit 7' TERM; touch /ready; sleep 300 & wait",
        );
        // Its trap set (a start returns before its shell runs it).
        let ready = stores.upper(&id).join("ready");
        eventually("its trap set", || ready.exists());
        // The marker the host's stop leaves (in the shared runtime root,
        // as root) goes whatever happens here.
        let _marker = HostStopping::at(&stores);
        let stopped = shipyard_engine::run::stop_for_the_host(&engine).unwrap();
        assert!(stopped.failed.is_empty(), "{:?}", stopped.failed);
        assert_eq!(stopped.stopped.len(), 1);
        assert!(
            matches!(
                stores.state(&id),
                ContainerState::Exited { code: Some(7), .. }
            ),
            "{:?}",
            stores.state(&id)
        );
        assert!(!stores.record(&id).stopped_by_user, "a user's stop");
        // Held off: not restarted, though it's `unless-stopped`.
        std::thread::sleep(Duration::from_secs(2));
        assert_eq!(stores.record(&id).restart_count, 0);
        assert!(matches!(stores.state(&id), ContainerState::Exited { .. }));
        let booted = shipyard_engine::run::boot(&engine).unwrap();
        assert_eq!(booted.started.len(), 1, "{:?}", booted.failed);
        assert!(matches!(stores.state(&id), ContainerState::Running { .. }));
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// A paused container's stop (F46): its stop signal is had at once, as
    /// the monitor thaws it, so its trap runs well within its timeout,
    /// where it would have waited the timeout out frozen, then been killed
    /// (137). The control: it was paused. Root: a pause needs the
    /// container's own cgroup. (`unless-stopped` only as the helper's
    /// policy: a user's stop holds it off.)
    fn a_paused_container_stops_gracefully() {
        if !is_root() {
            return;
        }
        let stores = Stores::new();
        let engine = stores.engine();
        let id = stores.launch_restarting(
            "unless-stopped",
            "trap 'exit 7' TERM; touch /ready; sleep 300 & wait",
        );
        let ready = stores.upper(&id).join("ready");
        eventually("its trap set", || ready.exists());
        runtime::pause(&stores.runtime_root(), id.as_str()).unwrap();
        assert!(
            matches!(stores.state(&id), ContainerState::Paused),
            "{:?}",
            stores.state(&id)
        );
        let began = Instant::now();
        shipyard_engine::run::stop(
            &engine,
            id.as_str(),
            shipyard_engine::run::StopOptions {
                timeout: Some(Duration::from_secs(60)),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            began.elapsed() < Duration::from_secs(10),
            "it waited its timeout: {:?}",
            began.elapsed()
        );
        assert!(
            matches!(
                stores.state(&id),
                ContainerState::Exited { code: Some(7), .. }
            ),
            "{:?}",
            stores.state(&id)
        );
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// A restart past its backoff, waiting for the container's operation
    /// lock (held here, as another command working on it would): the host
    /// starts stopping meanwhile, and once it has the lock it gives up
    /// rather than start the container in the shutdown (stage 9, step 9.9a,
    /// M3). Its user didn't stop it: `boot` brings it back.
    fn a_restart_waiting_for_its_lock_sees_the_host_stopping() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id = stores.launch_restarting("always", "sleep 1; exit 1");
        let held = nix::fcntl::Flock::lock(
            std::fs::File::open(
                containers::testing::record_dir(&engine, &id)
                    .unwrap()
                    .join("op.lock"),
            )
            .unwrap(),
            nix::fcntl::FlockArg::LockExclusive,
        )
        .unwrap();
        eventually("its restart waiting", || {
            matches!(stores.state(&id), ContainerState::Restarting { .. })
        });
        let waiter = Stores::restart_waiter(&id);
        // Past its backoff (its first: well under this), trying the lock.
        std::thread::sleep(Duration::from_secs(2));
        let guard = HostStopping::at(&stores);
        let marker = guard.0.clone();
        std::fs::write(&marker, b"").unwrap();
        drop(held);
        Stores::waiter_gone(waiter);
        let record = stores.record(&id);
        assert_eq!(record.restart_count, 0, "started in the shutdown");
        assert!(record.next_start.is_none(), "{:?}", record.next_start);
        assert!(!record.stopped_by_user);
        assert!(matches!(stores.state(&id), ContainerState::Exited { .. }));
        let booted = shipyard_engine::run::boot(&engine).unwrap();
        assert_eq!(booted.started.len(), 1, "{:?}", booted.failed);
        assert!(!marker.exists(), "boot removes it");
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// `update --restart no` ends a crash loop's restarts, the container
    /// kept.
    fn update_to_no_ends_the_restarts() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id = stores.launch_restarting("always", "sleep 1; exit 1");
        stores.slow_restart(&id);
        let waiter = Stores::restart_waiter(&id);
        let updated = shipyard_engine::run::update(
            &engine,
            id.as_str(),
            &shipyard_engine::run::UpdateRequest::restart(
                shipyard_engine::containers::RestartPolicy::No,
            ),
        )
        .unwrap();
        assert_eq!(
            updated.config.restart,
            shipyard_engine::containers::RestartPolicy::No
        );
        Stores::waiter_gone(waiter);
        assert!(matches!(stores.state(&id), ContainerState::Exited { .. }));
        assert!(stores.record(&id).next_start.is_none());
        shipyard_engine::run::remove(&engine, id.as_str(), false).unwrap();
    }

    /// `rm` refuses a container waiting to restart (it's up, as Docker
    /// sees it); `rm -f` removes it, and what waited gives up.
    fn rm_refuses_a_restarting_container() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id = stores.launch_restarting("always", "sleep 1; exit 1");
        stores.slow_restart(&id);
        let waiter = Stores::restart_waiter(&id);
        let refused = shipyard_engine::run::remove(&engine, id.as_str(), false).unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::InUse, "{refused:#}");
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
        Stores::waiter_gone(waiter);
        assert_eq!(
            containers::testing::find(&engine, id.as_str())
                .unwrap_err()
                .kind(),
            ErrorKind::NotFound
        );
    }

    /// What waited to restart a container, killed: the next pass leaves the
    /// container exited, not restarting for ever.
    fn a_killed_restart_waiter_is_settled() {
        let stores = Stores::new();
        let id = stores.launch_restarting("always", "sleep 1; exit 1");
        stores.slow_restart(&id);
        let waiter = Stores::restart_waiter(&id);
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(waiter),
            nix::sys::signal::Signal::SIGKILL,
        )
        .unwrap();
        Stores::waiter_gone(waiter);
        let (engine, seen) = stores.engine_collecting();
        let other =
            shipyard_engine::run::create(&engine, stores.request(&["true"]), false).unwrap();
        assert!(matches!(stores.state(&id), ContainerState::Exited { .. }));
        assert!(
            seen.warnings()
                .iter()
                .any(|w| w.kind == WarningKind::Recovered && w.message.contains("restart")),
            "{:?}",
            seen.warnings()
        );
        shipyard_engine::run::remove(&engine, other.id.as_str(), false).unwrap();
        shipyard_engine::run::remove(&engine, id.as_str(), false).unwrap();
    }

    /// `--rm` with a restart policy is refused (Docker's rule): a container
    /// removed as it ends can't be started again. Nothing is made.
    fn rm_with_a_restart_policy_is_refused() {
        let stores = Stores::new();
        let engine = stores.engine();
        let mut request = stores.request(&["true"]);
        request.restart = shipyard_engine::containers::RestartPolicy::Always;
        let refused = shipyard_engine::run::create(&engine, request, true).unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::InvalidRequest, "{refused:#}");
        assert!(containers::list(&engine).unwrap().is_empty());
    }

    /// A restart that can't start (its bind mount's source gone) counts as
    /// a failed run: `on-failure:2` tries twice more, then leaves it.
    fn a_failed_restart_counts_as_a_failure() {
        let stores = Stores::new();
        let source = stores.scratch.0.join("bound");
        std::fs::create_dir(&source).unwrap();
        let volume = format!("{}:/data", source.display());
        let id = Stores::id(&stores.launch(&[
            "--network",
            "none",
            "--restart",
            "on-failure:2",
            "--volume",
            &volume,
            stores.image.as_str(),
            "sh",
            "-c",
            "sleep 1; exit 1",
        ]));
        std::fs::remove_dir(&source).unwrap();
        eventually("its restarts used up", || {
            let record = stores.record(&id);
            record.restart_count == 2
                && record.next_start.is_none()
                && matches!(stores.state(&id), ContainerState::Exited { .. })
        });
        assert_eq!(
            stores.starts(&id),
            1,
            "a restart that couldn't start started"
        );
        shipyard_engine::run::remove(&stores.engine(), id.as_str(), false).unwrap();
    }

    /// A restart its policy makes keeps the latest ten starts' logs (its
    /// own among them): a crash loop doesn't fill the disk. (Older starts
    /// planted while it runs stand for the loop's.)
    fn a_restart_keeps_the_latest_ten_starts_logs() {
        let stores = Stores::new();
        let id = stores.launch_restarting("on-failure:1", "sleep 1; exit 1");
        let logs = containers::testing::logs_dir(&stores.engine(), &id).unwrap();
        let first = names(&logs);
        for older in 0..12 {
            std::fs::create_dir(logs.join(format!("{}", 1000 + older))).unwrap();
        }
        eventually("its restart", || stores.record(&id).restart_count == 1);
        eventually("its end", || {
            matches!(stores.state(&id), ContainerState::Exited { .. })
        });
        let kept = names(&logs);
        assert_eq!(kept.len(), 10, "{kept:?}");
        assert!(!kept.contains(&first[0]), "the oldest kept: {kept:?}");
        shipyard_engine::run::remove(&stores.engine(), id.as_str(), false).unwrap();
    }

    /// A policy restart, made by a clean-up that has its log open, never
    /// moves that log aside (stage 9, step 9.9a, D2): its next lines would
    /// follow the file into `cleanup.log.1`. A log grown past the limit
    /// while it waits is rotated by the next clean-up, as it opens it.
    fn a_restart_leaves_the_open_cleanup_log() {
        let stores = Stores::new();
        // Fails once, then runs: its writable layer keeps the mark.
        let id = stores.launch_restarting(
            "on-failure",
            "[ -e /ran ] && exec sleep 300; touch /ran; exit 1",
        );
        stores.slow_restart(&id);
        let dir = stores.scratch.0.join("containers").join(id.as_str());
        let log = dir.join("cleanup.log");
        // Past the limit, after its clean-up opened it.
        let mut grown = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&log)
            .unwrap();
        std::io::Write::write_all(&mut grown, ".".repeat(70 * 1024).as_bytes()).unwrap();
        drop(grown);
        eventually("restarted and running", || {
            stores.record(&id).restart_count == 1
                && matches!(stores.state(&id), ContainerState::Running { .. })
        });
        assert!(
            !dir.join("cleanup.log.1").exists(),
            "moved aside while open"
        );
        assert!(std::fs::metadata(&log).unwrap().len() >= 70 * 1024);
        shipyard_engine::run::remove(&stores.engine(), id.as_str(), true).unwrap();
    }

    /// The exit command is the binary that made the container (in its
    /// monitor, `/proc/self/exe`): removed since, it still cleans up.
    fn an_upgraded_binary_still_cleans_up() {
        let stores = Stores::new();
        let copy = stores.scratch.0.join("shipyard-old");
        std::fs::copy(std::env::current_exe().unwrap(), &copy).unwrap();
        let output = stores.launch_with(&copy, &[stores.image.as_str(), "sleep", "300"]);
        assert!(output.status.success(), "{output:?}");
        let id = Stores::id(&output);
        std::fs::remove_file(&copy).unwrap();
        runtime::kill(
            &stores.runtime_root(),
            id.as_str(),
            shipyard_runtime::process::Signal::KILL,
        )
        .unwrap();
        eventually("the record's exit", || stores.record(&id).exit.is_some());
        // The area went before the record was written, or failed and said
        // why.
        assert_eq!(stores.cleanup_log(&id), "", "nothing left for later");
        areas_let_go(&stores, &id);
    }

    /// A detached container's pasta (rootless's default network; root's when
    /// asked) runs on once the "CLI" has gone, followed with the container's
    /// init: its launcher in a session of its own, adopted (the "CLI" has no
    /// child left), pasta deaf to nothing; the published port answers from
    /// the host; killed, pasta goes, and the port, the run directory with it.
    fn a_pasta_container_follows_its_init() {
        if !pasta_here() {
            return;
        }
        let stores = Stores::new();
        // As root, in the tests' own network namespace: a way out for pasta
        // to copy the routes of (it can't set up a container without one).
        let _upstream = is_root().then(|| {
            let upstream = shipyard_test_support::net::Upstream::new();
            upstream.route_default();
            upstream
        });
        let before = stores.run_dirs();
        let port = free_port();
        let publish = format!("{port}:80");
        let output = stores.launch(&[
            "--network",
            "pasta",
            "--linger",
            "--publish",
            &publish,
            stores.image.as_str(),
            "sleep",
            "300",
        ]);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "{output:?}");
        assert!(
            stdout.contains("children=none"),
            "the CLI has no child left: {stdout}"
        );
        let id = Stores::id(&output);
        let init = runtime::state(&stores.runtime_root(), id.as_str())
            .unwrap()
            .pid
            .unwrap();
        let pasta = stores.pasta_pid(&before).expect("pasta.pid");
        let launcher = parent(pasta);
        assert_ne!(
            session(launcher),
            session(i32::try_from(std::process::id()).unwrap())
        );
        assert_eq!(blocked_signals(pasta), "0000000000000000", "pasta's mask");
        let ignored = u64::from_str_radix(&status_field(pasta, "SigIgn:"), 16).unwrap();
        assert_eq!(
            ignored & 0x4003,
            0,
            "pasta ignores SIGHUP, SIGINT or SIGTERM: {ignored:x}"
        );
        // Pasta and its launcher run in the monitor's cgroup, not the
        // caller's, so a scope ending ends neither (stage 9, step 9.9c,
        // R7); and where the monitor has a cgroup of its own, that isn't
        // this test's.
        let cgroup = |pid: i32| std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
        let monitor = parent(init);
        assert_eq!(cgroup(pasta), cgroup(monitor), "pasta's cgroup");
        assert_eq!(cgroup(launcher), cgroup(monitor), "its launcher's");
        if cgroup(monitor).contains("shipyard/monitors/") {
            assert_ne!(
                cgroup(monitor),
                cgroup(i32::try_from(std::process::id()).unwrap())
            );
        }
        // A server in the container: the published port reaches it.
        let mut server = Command::new(std::env::current_exe().unwrap())
            .args(["serve", &init.to_string(), "80", "hello-pasta"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut said = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(server.stdout.take().unwrap()),
            &mut said,
        )
        .unwrap();
        assert_eq!(said.trim(), "listening");
        let mut answer = String::new();
        let mut connection = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        connection
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        std::io::Read::read_to_string(&mut connection, &mut answer).unwrap();
        assert_eq!(answer, "hello-pasta");
        let _ = server.kill();
        let _ = server.wait();
        // Ended: pasta and its launcher go with the init.
        runtime::kill(
            &stores.runtime_root(),
            id.as_str(),
            shipyard_runtime::process::Signal::KILL,
        )
        .unwrap();
        let killed = Instant::now();
        eventually("pasta gone", || {
            !Path::new(&format!("/proc/{pasta}")).exists()
        });
        assert!(
            killed.elapsed() < Duration::from_secs(5),
            "{:?}",
            killed.elapsed()
        );
        eventually("its launcher gone", || {
            !Path::new(&format!("/proc/{launcher}")).exists()
        });
        eventually("the record's exit", || stores.record(&id).exit.is_some());
        eventually("the port free", || bindable(port));
        eventually("the run directory gone", || {
            stores.run_dirs().iter().all(|dir| before.contains(dir))
        });
        assert_eq!(stores.cleanup_log(&id), "", "nothing left for later");
        // pasta went with the init: not a loss, nothing said.
        assert_eq!(
            containers::last_start_lost(&stores.engine(), &stores.record(&id)),
            None
        );
        assert!(
            !stores
                .actions(id.as_str())
                .contains(&"network_lost".to_owned())
        );
    }

    /// A pasta that fails as it starts: `detached` says so, the container
    /// is ended (never started), and nothing is held.
    fn a_pasta_that_fails_leaves_nothing() {
        let stores = Stores::new();
        let before = stores.run_dirs();
        let output = stores.launch_with_fake_pasta(
            "echo 'no network for you' >&2; exit 3",
            &["--network", "pasta", stores.image.as_str(), "sleep", "300"],
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("error=Network"), "{output:?}");
        assert!(
            stdout.contains("no network for you"),
            "pasta's words: {stdout}"
        );
        let records = containers::list(&stores.engine()).unwrap();
        assert_eq!(records.len(), 1);
        let id = records[0].id.clone();
        eventually("the clean-up done", || {
            stores.running_file(&id).try_lock().is_ok()
        });
        let record = stores.record(&id);
        assert_eq!(
            (record.started, record.finished, record.exit),
            (None, None, None),
            "never started (F64)"
        );
        assert_eq!(stores.state(&id), ContainerState::Created);
        areas_let_go(&stores, &id);
        // Root's runtime root is shared: only this run's directory counts.
        eventually("the run directory gone", || {
            stores.run_dirs().iter().all(|dir| before.contains(dir))
        });
    }

    /// A pasta never ready (its process ID file never written): its
    /// launcher ends it after its wait, and says so; nothing is left, its
    /// launcher neither.
    fn a_pasta_never_ready_is_ended() {
        let stores = Stores::new();
        let before = stores.run_dirs();
        let launchers = processes_with(shipyard_network::pasta::LAUNCHER_ARG);
        let started = Instant::now();
        let output = stores.launch_with_fake_pasta(
            "exec sleep 61",
            &["--network", "pasta", stores.image.as_str(), "sleep", "300"],
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("wasn't ready"), "{output:?}");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{:?}",
            started.elapsed()
        );
        eventually("the fake pasta gone", || {
            processes_with("sleep 61").is_empty()
        });
        let id = containers::list(&stores.engine()).unwrap()[0].id.clone();
        eventually("the clean-up done", || {
            stores.running_file(&id).try_lock().is_ok()
        });
        let record = stores.record(&id);
        assert_eq!(
            (record.started, record.finished, record.exit),
            (None, None, None),
            "never started (F64)"
        );
        assert_eq!(stores.state(&id), ContainerState::Created);
        areas_let_go(&stores, &id);
        eventually("the run directory gone", || {
            stores.run_dirs().iter().all(|dir| before.contains(dir))
        });
        eventually("no launcher left", || {
            processes_with(shipyard_network::pasta::LAUNCHER_ARG)
                .iter()
                .all(|launcher| launchers.contains(launcher))
        });
    }

    /// The init killed while pasta starts: its launcher sees it (the
    /// init's pidfd), ends pasta and says why; `detached` fails at once,
    /// not at pasta's deadline.
    fn the_init_killed_while_pasta_starts_ends_it() {
        let stores = Stores::new();
        let cli = stores
            .with_pasta_script(
                "case \"$*\" in *--help*) echo '  -I, --ns-ifname NAME'; exit 0;; esac\n\
                 exec sleep 62",
                &["--network", "pasta", stores.image.as_str(), "sleep", "300"],
            )
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        eventually("the fake pasta started", || {
            !processes_with("sleep 62").is_empty()
        });
        let id = containers::list(&stores.engine()).unwrap()[0].id.clone();
        let init = runtime::state(&stores.runtime_root(), id.as_str())
            .unwrap()
            .pid
            .unwrap();
        let killed = Instant::now();
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(init),
            nix::sys::signal::Signal::SIGKILL,
        )
        .unwrap();
        let output = cli.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("init ended"), "{output:?}");
        assert!(
            killed.elapsed() < Duration::from_secs(3),
            "{:?}",
            killed.elapsed()
        );
        eventually("the fake pasta gone", || {
            processes_with("sleep 62").is_empty()
        });
        eventually("the clean-up done", || {
            stores.running_file(&id).try_lock().is_ok()
        });
        let record = stores.record(&id);
        assert_eq!(
            (record.started, record.finished, record.exit),
            (None, None, None),
            "never started (F64)"
        );
        areas_let_go(&stores, &id);
    }

    /// A pasta container, its pasta (the real one) started without a
    /// parent-death signal, as under SELinux; its launcher found. The
    /// `_upstream` returned keeps root's way out.
    fn undying_pasta(
        stores: &Stores,
        before: &[String],
    ) -> (
        ContainerId,
        i32,
        i32,
        Option<shipyard_test_support::net::Upstream>,
    ) {
        let upstream = is_root().then(|| {
            let upstream = shipyard_test_support::net::Upstream::new();
            upstream.route_default();
            upstream
        });
        let output = stores
            .with_pasta_undying(&["--network", "pasta", stores.image.as_str(), "sleep", "300"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let id = Stores::id(&output);
        let pasta = stores.pasta_pid(before).expect("pasta.pid");
        (id, pasta, parent(pasta), upstream)
    }

    /// Its launcher killed outright (under SELinux, pasta lives on: its
    /// parent-death signal cleared): pasta still ends once `__cleanup`
    /// removes the run's directory, its network namespace file (no
    /// `--no-netns-quit` for a detached run's pasta).
    fn a_pasta_whose_launcher_was_killed_ends_with_the_clean_up() {
        if !pasta_here() {
            return;
        }
        let stores = Stores::new();
        let before = stores.run_dirs();
        let (id, pasta, launcher, _upstream) = undying_pasta(&stores, &before);
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(launcher),
            nix::sys::signal::Signal::SIGKILL,
        )
        .unwrap();
        eventually("the launcher gone", || {
            !Path::new(&format!("/proc/{launcher}")).exists()
        });
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            Path::new(&format!("/proc/{pasta}")).exists(),
            "pasta died with its launcher: the test shows nothing"
        );
        runtime::kill(
            &stores.runtime_root(),
            id.as_str(),
            shipyard_runtime::process::Signal::KILL,
        )
        .unwrap();
        eventually("the run directory gone", || {
            stores.run_dirs().iter().all(|dir| before.contains(dir))
        });
        let gone = Instant::now();
        eventually("pasta gone", || {
            !Path::new(&format!("/proc/{pasta}")).exists()
        });
        assert!(
            gone.elapsed() < Duration::from_secs(5),
            "{:?}",
            gone.elapsed()
        );
    }

    /// Its launcher told to stop by any of the usual signals (SIGINT here,
    /// as a sweep might send): pasta goes first, even one that wouldn't die
    /// with it.
    fn a_launcher_interrupted_ends_its_pasta() {
        if !pasta_here() {
            return;
        }
        let stores = Stores::new();
        let before = stores.run_dirs();
        let (id, pasta, launcher, _upstream) = undying_pasta(&stores, &before);
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(launcher),
            nix::sys::signal::Signal::SIGINT,
        )
        .unwrap();
        let told = Instant::now();
        eventually("pasta gone", || {
            !Path::new(&format!("/proc/{pasta}")).exists()
        });
        assert!(
            told.elapsed() < Duration::from_secs(1),
            "{:?}",
            told.elapsed()
        );
        runtime::kill(
            &stores.runtime_root(),
            id.as_str(),
            shipyard_runtime::process::Signal::KILL,
        )
        .unwrap();
        eventually("the run directory gone", || {
            stores.run_dirs().iter().all(|dir| before.contains(dir))
        });
    }

    /// A detached pasta keeps none of the host's filesystems alive: the
    /// host unmounts one (here a tmpfs, the "CLI" started in it), and its
    /// launcher's copy of the mounts lets go too (slaves, and its working
    /// directory `/`). Root: only root mounts on the host.
    fn a_pasta_launcher_pins_no_host_mount() {
        if !is_root() || !pasta_here() {
            return;
        }
        let stores = Stores::new();
        let _upstream = {
            let upstream = shipyard_test_support::net::Upstream::new();
            upstream.route_default();
            upstream
        };
        let before = stores.run_dirs();
        let disk = stores.scratch.0.join("disk");
        std::fs::create_dir_all(&disk).unwrap();
        nix::mount::mount(
            Some("tmpfs"),
            &disk,
            Some("tmpfs"),
            nix::mount::MsFlags::empty(),
            None::<&str>,
        )
        .unwrap();
        let output = stores
            .with_pasta_script(
                &format!(
                    "exec {} \"$@\"",
                    shipyard_network::pasta::find(std::env::var_os("PATH").as_deref())
                        .unwrap()
                        .display()
                ),
                &["--network", "pasta", stores.image.as_str(), "sleep", "300"],
            )
            .current_dir(&disk)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let id = Stores::id(&output);
        let launcher = parent(stores.pasta_pid(&before).expect("pasta.pid"));
        nix::mount::umount(&disk).expect("the host's unmount (busy: something pins it)");
        let mounts = std::fs::read_to_string(format!("/proc/{launcher}/mountinfo")).unwrap();
        assert!(
            !mounts.contains(disk.to_str().unwrap()),
            "the launcher keeps the unmounted filesystem: {mounts}"
        );
        runtime::kill(
            &stores.runtime_root(),
            id.as_str(),
            shipyard_runtime::process::Signal::KILL,
        )
        .unwrap();
        eventually("the run directory gone", || {
            stores.run_dirs().iter().all(|dir| before.contains(dir))
        });
        eventually("its launcher gone", || {
            !Path::new(&format!("/proc/{launcher}")).exists()
        });
    }

    /// A container starts again: what it wrote is still there (its own
    /// area, kept while it's stopped, unlocked), its last end is cleared at
    /// the start, and each start has its own log directory. Removed, its
    /// area goes.
    fn a_container_starts_again_with_its_files() {
        let stores = Stores::new();
        let engine = stores.engine();
        let record = shipyard_engine::run::create(
            &engine,
            stores.request(&["sh", "-c", "echo x >> /data.txt; cat /data.txt; sleep 300"]),
            false,
        )
        .unwrap();
        let id = record.id.clone();
        assert_eq!(stores.state(&id), ContainerState::Created);
        assert!(record.image_id.is_some(), "the image's digest recorded");
        assert!(stores.areas().is_empty(), "no area until it starts");
        shipyard_engine::run::start(&engine, id.as_str()).unwrap();
        assert!(matches!(stores.state(&id), ContainerState::Running { .. }));
        let upper = stores.upper(&id);
        eventually("the file written", || upper.join("data.txt").exists());
        stores.kill_and_wait(&id);
        assert!(
            matches!(
                stores.state(&id),
                ContainerState::Exited {
                    code: Some(137),
                    ..
                }
            ),
            "{:?}",
            stores.state(&id)
        );
        areas_let_go(&stores, &id);
        assert!(upper.join("data.txt").exists(), "kept while it's stopped");
        shipyard_engine::run::start(&engine, id.as_str()).unwrap();
        assert_eq!(stores.record(&id).exit, None, "its last end cleared");
        let logs = containers::testing::logs_dir(&engine, &id).unwrap();
        eventually("the second start's log", || {
            std::fs::read_to_string(logs.join("0000000002").join("container.log"))
                .is_ok_and(|log| log.matches(" x").count() == 2)
        });
        assert_eq!(
            std::fs::read_to_string(upper.join("data.txt")).unwrap(),
            "x\nx\n"
        );
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
        assert!(!upper.exists(), "the area goes with it");
        assert!(stores.areas().is_empty());
    }

    /// A start right after the container's own end (its `__cleanup` may
    /// still be letting go) waits for it, then starts: no runtime state
    /// left in the way, no end of the last start written over this one.
    fn a_start_right_after_its_own_end_works() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id =
            shipyard_engine::run::create(&engine, stores.request(&["sh", "-c", "exit 3"]), false)
                .unwrap()
                .id;
        for _ in 0..3 {
            shipyard_engine::run::start(&engine, id.as_str()).unwrap();
            // Ended (its monitor said so), its clean-up perhaps still at
            // work: the next start waits for it.
            eventually("it ended", || {
                !matches!(
                    runtime::status(&stores.runtime_root(), id.as_str()).unwrap(),
                    Some(runtime::Status::Running | runtime::Status::Created)
                )
            });
        }
        eventually("an end recorded", || {
            stores
                .record(&id)
                .exit
                .is_some_and(|exit| exit.code == Some(3))
        });
        let logs = containers::testing::logs_dir(&engine, &id).unwrap();
        assert_eq!(names(&logs).len(), 3, "three starts");
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// A start's `running` file is free only once everything of it is let
    /// go and settled: the moment it is, the runtime state is gone and the
    /// end recorded, so the next start, which takes it, never races the last
    /// one's `__cleanup`. (Watched without a pause: a window of a
    /// millisecond would show.)
    fn running_is_free_only_once_its_end_is_settled() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id =
            shipyard_engine::run::create(&engine, stores.request(&["sh", "-c", "exit 3"]), false)
                .unwrap()
                .id;
        for _ in 0..3 {
            shipyard_engine::run::start(&engine, id.as_str()).unwrap();
            let running = stores.running_file(&id);
            let deadline = Instant::now() + Duration::from_secs(20);
            while running.try_lock().is_err() {
                assert!(Instant::now() < deadline, "never let go");
            }
            assert!(
                runtime::status(&stores.runtime_root(), id.as_str())
                    .unwrap()
                    .is_none(),
                "free, its runtime state still there"
            );
            assert!(
                stores
                    .record(&id)
                    .exit
                    .is_some_and(|exit| exit.code == Some(3)),
                "free, its end not recorded: {:?}",
                stores.record(&id)
            );
        }
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// With pasta: once the container's `running` file is free (its end
    /// settled), its pasta has let go of the published port, so a start
    /// again can bind it (pasta's launcher holds `running` until pasta is
    /// reaped). Started twice: the second is the start that found the port
    /// still taken.
    fn its_pastas_ports_are_free_once_its_end_is_settled() {
        // Rootless, pasta's use: as root, the tests' own network namespace
        // has no route for pasta to give the container.
        if is_root() || !pasta_here() {
            return;
        }
        let stores = Stores::new();
        let engine = stores.engine();
        let port = free_port();
        let mut request = stores.request(&["sleep", "300"]);
        request.network = vec![NetworkMode::Pasta];
        request.publish = shipyard_network::ports::parse_publish(&format!("{port}:80")).unwrap();
        let id = shipyard_engine::run::create(&engine, request, false)
            .unwrap()
            .id;
        for start in 0..2 {
            shipyard_engine::run::start(&engine, id.as_str()).unwrap();
            assert!(!bindable(port), "published");
            // Clients served (pasta takes each, finds nothing listening
            // inside, and closes first, or resets it): until one has left
            // the host's side in TIME_WAIT on the port, which the next start
            // must hold anyway (F42).
            let time_wait = || {
                let table = std::fs::read_to_string("/proc/net/tcp").unwrap();
                shipyard_network::observe::parse(&table, shipyard_network::observe::Table::Tcp)
                    .unwrap()
                    .iter()
                    .any(|socket| {
                        socket.local.port() == port
                            && socket.state == shipyard_network::observe::State::TimeWait
                    })
            };
            for _ in 0..10 {
                let mut client = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
                client
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let _ = std::io::Read::read_to_end(&mut client, &mut Vec::new());
                drop(client);
                if time_wait() {
                    break;
                }
            }
            runtime::kill(
                &stores.runtime_root(),
                id.as_str(),
                shipyard_runtime::process::Signal::KILL,
            )
            .unwrap();
            let running = stores.running_file(&id);
            eventually("its end settled", || running.try_lock().is_ok());
            assert!(bindable(port), "free, its pasta still holds the port");
            if start == 0 {
                assert!(
                    time_wait(),
                    "no TIME_WAIT on the port: the next start shows nothing"
                );
            }
        }
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// A start waits while the container's `running` file is held (its last
    /// start's `__cleanup` still at work), then starts.
    fn a_start_waits_for_its_last_clean_up() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id = shipyard_engine::run::create(&engine, stores.request(&["sleep", "300"]), false)
            .unwrap()
            .id;
        let running = stores.running_file(&id);
        running.lock().unwrap();
        let held = Duration::from_secs(1);
        let release = std::thread::spawn(move || {
            std::thread::sleep(held);
            drop(running);
        });
        let began = Instant::now();
        shipyard_engine::run::start(&engine, id.as_str()).unwrap();
        let took = began.elapsed();
        release.join().unwrap();
        assert!(took >= held, "started while held: {took:?}");
        assert!(matches!(stores.state(&id), ContainerState::Running { .. }));
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// F5: a container that ran once, whose next start was killed before
    /// its gate opened, then ended there: its command never ran this time,
    /// so no end is recorded for it (no `Exited (137)`). It keeps what its
    /// last start left (9.9a, the whole-stage review's H2): exited, with
    /// that start's code.
    fn an_end_before_the_gate_records_no_exit() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id =
            shipyard_engine::run::create(&engine, stores.request(&["sh", "-c", "exit 0"]), false)
                .unwrap()
                .id;
        shipyard_engine::run::start(&engine, id.as_str()).unwrap();
        eventually("its first end", || {
            runtime::status(&stores.runtime_root(), id.as_str())
                .unwrap()
                .is_none()
                && stores.running_file(&id).try_lock().is_ok()
        });
        let aborted = stores
            .container_command("start-abort", id.as_str())
            .output()
            .unwrap();
        assert!(!aborted.status.success(), "{aborted:?}");
        assert_eq!(
            runtime::status(&stores.runtime_root(), id.as_str()).unwrap(),
            Some(runtime::Status::Created),
            "left at the gate"
        );
        runtime::delete(&stores.runtime_root(), id.as_str(), true).unwrap();
        eventually("its clean-up done", || {
            stores.running_file(&id).try_lock().is_ok()
        });
        let record = stores.record(&id);
        assert!(
            matches!(
                stores.state(&id),
                ContainerState::Exited { code: Some(0), .. }
            ),
            "{record:?}"
        );
        assert!(
            record
                .exit
                .as_ref()
                .is_none_or(|exit| exit.code != Some(137)),
            "{record:?}"
        );
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// Its monitor killed outright (no `__cleanup`, its runtime state
    /// left): the next start settles what's left, and starts it.
    fn a_start_after_its_monitor_was_killed_works() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id = shipyard_engine::run::create(&engine, stores.request(&["sleep", "300"]), false)
            .unwrap()
            .id;
        shipyard_engine::run::start(&engine, id.as_str()).unwrap();
        let init = runtime::state(&stores.runtime_root(), id.as_str())
            .unwrap()
            .pid
            .unwrap();
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(parent(init)),
            nix::sys::signal::Signal::SIGKILL,
        )
        .unwrap();
        eventually("its init gone with its monitor", || {
            !Path::new(&format!("/proc/{init}")).exists()
        });
        shipyard_engine::run::start(&engine, id.as_str()).unwrap();
        assert!(
            matches!(stores.state(&id), ContainerState::Running { .. }),
            "{:?}",
            stores.state(&id)
        );
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
        assert!(stores.areas().is_empty());
    }

    /// A start that fails (its published port taken) keeps what the
    /// container wrote: its own area is only let go, never removed.
    fn a_failed_start_keeps_the_containers_files() {
        if !is_root() {
            return;
        }
        let stores = Stores::new();
        let engine = stores.engine();
        let port = free_port();
        let mut request = stores.request(&["sh", "-c", "echo kept > /data.txt; sleep 300"]);
        request.network = Vec::new();
        request.publish = shipyard_network::ports::parse_publish(&format!("{port}:80")).unwrap();
        let id = shipyard_engine::run::create(&engine, request, false)
            .unwrap()
            .id;
        shipyard_engine::run::start(&engine, id.as_str()).unwrap();
        let file = stores.upper(&id).join("data.txt");
        eventually("the file written", || file.exists());
        stores.kill_and_wait(&id);
        let taken = std::net::TcpListener::bind(("0.0.0.0", port)).unwrap();
        let refused = shipyard_engine::run::start(&engine, id.as_str()).unwrap_err();
        assert_eq!(
            refused.kind(),
            shipyard_engine::ErrorKind::Network,
            "{refused:#}"
        );
        drop(taken);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "kept\n");
        areas_let_go(&stores, &id);
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// The image a container was made from stays while it exists (its
    /// pin): `rmi` refuses it, `rmi -f` and a prune can't take its files,
    /// and its start (by digest) records no new reference.
    fn the_image_stays_with_its_container() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id =
            shipyard_engine::run::create(&engine, stores.request(&["sh", "-c", "exit 0"]), false)
                .unwrap()
                .id;
        let refused = shipyard_engine::image::remove(&engine, &stores.image, false).unwrap_err();
        assert_eq!(
            refused.kind(),
            shipyard_engine::ErrorKind::InUse,
            "{refused:#}"
        );
        assert!(refused.to_string().contains(id.short()), "{refused:#}");
        shipyard_engine::image::remove(&engine, &stores.image, true).unwrap();
        shipyard_engine::image::prune(&engine).unwrap();
        let before = shipyard_engine::image::list(&engine).unwrap();
        assert!(before.is_empty(), "untagged, and the pin isn't listed");
        shipyard_engine::run::start(&engine, id.as_str()).unwrap();
        eventually("its end", || stores.record(&id).exit.is_some());
        assert_eq!(stores.record(&id).exit.unwrap().code, Some(0));
        assert_eq!(
            shipyard_engine::image::list(&engine).unwrap(),
            before,
            "no reference recorded by the start"
        );
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
        shipyard_engine::image::prune(&engine).unwrap();
        assert!(
            !std::fs::read_to_string(stores.scratch.store().join("index.json"))
                .unwrap_or_default()
                .contains("container:"),
            "its pin gone"
        );
    }

    /// Two container stores on one image store: one's runs, sweeps and
    /// prunes never take the other's stopped containers' areas (each area
    /// says which store it belongs to).
    fn two_container_stores_share_an_image_store() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id =
            shipyard_engine::run::create(&engine, stores.request(&["sh", "-c", "exit 0"]), false)
                .unwrap()
                .id;
        shipyard_engine::run::start(&engine, id.as_str()).unwrap();
        eventually("its end", || stores.record(&id).exit.is_some());
        areas_let_go(&stores, &id);
        let area = format!("ctr-{}", id.as_str());
        // Another store, the same image store.
        let other_dir = stores.scratch.0.join("other-containers");
        std::fs::create_dir_all(&other_dir).unwrap();
        std::fs::set_permissions(&other_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let other = Engine::new(
            engine.paths().clone().with_containers(other_dir),
            Arc::new(Collect::default()) as Arc<dyn Reporter>,
        );
        let theirs =
            shipyard_engine::run::create(&other, stores.request(&["sh", "-c", "exit 0"]), false)
                .unwrap()
                .id;
        shipyard_engine::run::start(&other, theirs.as_str()).unwrap();
        shipyard_engine::image::prune(&other).unwrap();
        assert!(
            stores.areas().contains(&area),
            "the first store's area kept"
        );
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
        eventually("theirs ended", || {
            containers::testing::find(&other, theirs.as_str())
                .unwrap()
                .exit
                .is_some()
        });
        shipyard_engine::run::remove(&other, theirs.as_str(), true).unwrap();
    }

    /// A start and a forced removal of one container, at once, from two
    /// processes: they never interleave, so what's left is whole (gone, or
    /// a container that's there with all it needs), never half of each.
    /// Two starts at once start it once.
    fn start_and_rm_never_interleave() {
        let stores = Stores::new();
        let engine = stores.engine();
        for _ in 0..3 {
            let id =
                shipyard_engine::run::create(&engine, stores.request(&["sleep", "300"]), false)
                    .unwrap()
                    .id;
            let start = stores
                .container_command("start", id.as_str())
                .spawn()
                .unwrap();
            let rm = stores.container_command("rm", id.as_str()).spawn().unwrap();
            let (start, rm) = (
                start.wait_with_output().unwrap(),
                rm.wait_with_output().unwrap(),
            );
            assert!(rm.status.success(), "{rm:?}");
            // Started (it went first: then killed), or not found (the
            // removal went first); never half of each.
            assert!(
                start.status.success()
                    || String::from_utf8_lossy(&start.stdout).contains("error=NotFound"),
                "{start:?}"
            );
            assert!(
                containers::testing::find(&engine, id.as_str()).is_err(),
                "removed"
            );
            assert!(
                runtime::status(&stores.runtime_root(), id.as_str())
                    .unwrap()
                    .is_none(),
                "no runtime state left"
            );
            assert!(
                !containers::testing::record_dir(&engine, &id)
                    .unwrap()
                    .exists(),
                "no directory left"
            );
            eventually("no area left", || {
                !stores.areas().contains(&format!("ctr-{}", id.as_str()))
            });
        }
        let id = shipyard_engine::run::create(&engine, stores.request(&["sleep", "300"]), false)
            .unwrap()
            .id;
        let first = stores
            .container_command("start", id.as_str())
            .spawn()
            .unwrap();
        let second = stores
            .container_command("start", id.as_str())
            .spawn()
            .unwrap();
        for started in [
            first.wait_with_output().unwrap(),
            second.wait_with_output().unwrap(),
        ] {
            assert!(started.status.success(), "{started:?}");
        }
        let logs = containers::testing::logs_dir(&engine, &id).unwrap();
        assert_eq!(names(&logs).len(), 1, "started once");
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// `run -d --rm`: once it ends, nothing of it is left (its record, its
    /// area, its image's pin).
    fn run_d_rm_leaves_nothing() {
        let stores = Stores::new();
        let output = stores.launch(&[
            "--rm",
            "--network",
            "none",
            stores.image.as_str(),
            "sh",
            "-c",
            "exit 3",
        ]);
        assert!(output.status.success(), "{output:?}");
        let id = Stores::id(&output);
        eventually("the container gone", || {
            containers::testing::find(&stores.engine(), id.as_str()).is_err()
        });
        eventually("its area gone", || stores.areas().is_empty());
        assert!(
            !std::fs::read_to_string(stores.scratch.store().join("index.json"))
                .unwrap_or_default()
                .contains("container:"),
            "its pin gone"
        );
    }

    /// A container made with a terminal and input (`create -it`) starts
    /// from a caller that has neither (a script, cron): its streams are its
    /// monitor's, never the caller's terminal.
    fn a_terminal_container_starts_from_a_script() {
        let stores = Stores::new();
        let engine = stores.engine();
        let mut request = stores.request(&["sleep", "300"]);
        request.streams.tty = true;
        request.streams.interactive = true;
        let id = shipyard_engine::run::create(&engine, request, false)
            .unwrap()
            .id;
        shipyard_engine::run::start(&engine, id.as_str()).unwrap();
        assert!(matches!(stores.state(&id), ContainerState::Running { .. }));
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// A start killed outright once the container is made and before it's
    /// started (its monitor waits at the gate, holding everything): the
    /// next `start` ends that dead start's container and starts it; `rm`
    /// works too.
    fn a_start_killed_before_its_end_leaves_it_startable() {
        let stores = Stores::new();
        let engine = stores.engine();
        for removed in [false, true] {
            let output = stores.launch(&[
                "--abort-between",
                "--network",
                "none",
                stores.image.as_str(),
                "sleep",
                "300",
            ]);
            assert!(!output.status.success(), "{output:?}");
            let id = containers::list(&engine).unwrap()[0].id.clone();
            assert_eq!(
                runtime::status(&stores.runtime_root(), id.as_str()).unwrap(),
                Some(runtime::Status::Created),
                "left at the gate"
            );
            if removed {
                shipyard_engine::run::remove(&engine, id.as_str(), false).unwrap();
            } else {
                shipyard_engine::run::start(&engine, id.as_str()).unwrap();
                assert!(matches!(stores.state(&id), ContainerState::Running { .. }));
                shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
            }
            assert!(containers::list(&engine).unwrap().is_empty());
            assert!(stores.areas().is_empty());
        }
    }

    /// `rm` while another thread lists the image store's areas (each probed
    /// by taking its lock for an instant: `system df`, another start's
    /// sweep): never "in use".
    fn rm_beside_a_listing_of_the_areas() {
        let stores = Stores::new();
        let engine = stores.engine();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let lister = {
            let stop = Arc::clone(&stop);
            let engine = stores.engine();
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = shipyard_engine::image::disk_usage(&engine);
                }
            })
        };
        for _ in 0..5 {
            let id = shipyard_engine::run::create(
                &engine,
                stores.request(&["sh", "-c", "exit 0"]),
                false,
            )
            .unwrap()
            .id;
            shipyard_engine::run::start(&engine, id.as_str()).unwrap();
            eventually("its end", || stores.record(&id).exit.is_some());
            shipyard_engine::run::remove(&engine, id.as_str(), false).unwrap();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        lister.join().unwrap();
        assert!(stores.areas().is_empty());
    }

    /// A command that won't end on SIGTERM (its shell traps it, and goes
    /// on), saying so once it does (`/deaf`: a stop sent before then would
    /// still end it), and saying when one came (`/termed`).
    const DEAF: &[&str] = &[
        "sh",
        "-c",
        "trap 'touch /termed' TERM; touch /deaf; while :; do sleep 1; done",
    ];

    /// Makes and starts a [`DEAF`] container, once it ignores SIGTERM.
    fn started_deaf(stores: &Stores, engine: &shipyard_engine::Engine) -> ContainerId {
        let id = shipyard_engine::run::create(engine, stores.request(DEAF), false)
            .unwrap()
            .id;
        shipyard_engine::run::start(engine, id.as_str()).unwrap();
        let deaf = stores.upper(&id).join("deaf");
        eventually("it ignores SIGTERM", || deaf.exists());
        id
    }

    /// Its exit code, as its record has it.
    fn exit_code(stores: &Stores, id: &ContainerId) -> Option<i32> {
        stores.record(id).exit.and_then(|exit| exit.code)
    }

    /// `stop`: its stop signal (SIGTERM), its end recorded (143) and
    /// settled once `stop` returns (its `running` free), so a `start` right
    /// after works; a `stop` of one not up does nothing.
    fn stop_ends_it_and_it_starts_again() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id = shipyard_engine::run::create(&engine, stores.request(&["sleep", "300"]), false)
            .unwrap()
            .id;
        shipyard_engine::run::start(&engine, id.as_str()).unwrap();
        let began = Instant::now();
        shipyard_engine::run::stop(
            &engine,
            id.as_str(),
            shipyard_engine::run::StopOptions::default(),
        )
        .unwrap();
        assert!(
            began.elapsed() < Duration::from_secs(5),
            "{:?}",
            began.elapsed()
        );
        assert!(stores.running_file(&id).try_lock().is_ok(), "not settled");
        assert_eq!(exit_code(&stores, &id), Some(143));
        shipyard_engine::run::start(&engine, id.as_str()).unwrap();
        assert!(matches!(stores.state(&id), ContainerState::Running { .. }));
        shipyard_engine::run::stop(
            &engine,
            id.as_str(),
            shipyard_engine::run::StopOptions::default(),
        )
        .unwrap();
        shipyard_engine::run::stop(
            &engine,
            id.as_str(),
            shipyard_engine::run::StopOptions::default(),
        )
        .unwrap();
        assert_eq!(exit_code(&stores, &id), Some(143));
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// One that ignores its stop signal gets SIGKILL once the timeout is up
    /// (137), the OOM killer not blamed.
    fn stop_kills_one_that_ignores_its_signal() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id = started_deaf(&stores, &engine);
        let began = Instant::now();
        let options = shipyard_engine::run::StopOptions {
            timeout: Some(Duration::from_secs(1)),
            ..Default::default()
        };
        shipyard_engine::run::stop(&engine, id.as_str(), options).unwrap();
        let took = began.elapsed();
        assert!(
            took >= Duration::from_secs(1) && took < Duration::from_secs(5),
            "{took:?}"
        );
        let record = stores.record(&id);
        let exit = record.exit.clone().unwrap();
        assert_eq!(exit.code, Some(137), "{record:?}");
        assert_eq!(exit.oom, None, "{record:?}");
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// `kill` takes no lock and doesn't wait: it ends a container a `stop`
    /// (another process's, 30 s to go) waits on, and that `stop` returns.
    /// A `kill` of one that isn't running says so.
    fn kill_reaches_one_a_stop_waits_on() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id = started_deaf(&stores, &engine);
        let stopping = stores
            .container_command("stop", id.as_str())
            .arg("30")
            .spawn()
            .unwrap();
        // Its SIGTERM has come: the `stop` holds the operation lock now.
        let termed = stores.upper(&id).join("termed");
        eventually("the stop's SIGTERM", || termed.exists());
        assert!(matches!(stores.state(&id), ContainerState::Running { .. }));
        let began = Instant::now();
        shipyard_engine::run::kill(
            &engine,
            id.as_str(),
            shipyard_runtime::process::Signal::KILL,
        )
        .unwrap();
        assert!(
            began.elapsed() < Duration::from_secs(1),
            "{:?}",
            began.elapsed()
        );
        let stopped = stopping.wait_with_output().unwrap();
        assert!(stopped.status.success(), "{stopped:?}");
        assert!(
            began.elapsed() < Duration::from_secs(10),
            "{:?}",
            began.elapsed()
        );
        assert_eq!(exit_code(&stores, &id), Some(137));
        let error = shipyard_engine::run::kill(
            &engine,
            id.as_str(),
            shipyard_runtime::process::Signal::KILL,
        )
        .unwrap_err();
        assert_eq!(
            error.kind(),
            shipyard_engine::ErrorKind::NotRunning,
            "{error:#}"
        );
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// `wait`: 0 for one never started (Docker's); the exit code once it
    /// has ended, to several waiters alike; at once for one that has.
    fn wait_gives_the_exit_code() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id = shipyard_engine::run::create(
            &engine,
            stores.request(&["sh", "-c", "sleep 1; exit 7"]),
            false,
        )
        .unwrap()
        .id;
        assert_eq!(shipyard_engine::run::wait(&engine, id.as_str()).unwrap(), 0);
        shipyard_engine::run::start(&engine, id.as_str()).unwrap();
        let waiters: Vec<_> = (0..2)
            .map(|_| {
                let engine = stores.engine();
                let id = id.clone();
                std::thread::spawn(move || shipyard_engine::run::wait(&engine, id.as_str()))
            })
            .collect();
        for waiter in waiters {
            assert_eq!(waiter.join().unwrap().unwrap(), 7);
        }
        assert_eq!(shipyard_engine::run::wait(&engine, id.as_str()).unwrap(), 7);
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// A `--rm` container's waiter is told its exit code, though the
    /// container goes as it ends.
    fn wait_reaches_an_rm_containers_end() {
        let stores = Stores::new();
        let engine = stores.engine();
        let detached = shipyard_engine::run::detached(
            &engine,
            stores.request(&["sh", "-c", "sleep 1; exit 3"]),
            true,
        )
        .unwrap();
        let id = detached.id.clone();
        drop(detached);
        assert_eq!(shipyard_engine::run::wait(&engine, id.as_str()).unwrap(), 3);
        eventually("removed", || {
            containers::testing::find(&engine, id.as_str()).is_err()
        });
    }

    /// `restart`: stopped, then started under the same hold: a second
    /// start (its own logs), what it wrote kept.
    fn restart_stops_then_starts() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id = shipyard_engine::run::create(
            &engine,
            stores.request(&["sh", "-c", "echo started >> /starts; sleep 300"]),
            false,
        )
        .unwrap()
        .id;
        shipyard_engine::run::start(&engine, id.as_str()).unwrap();
        let starts = stores.upper(&id).join("starts");
        eventually("its first start", || starts.exists());
        shipyard_engine::run::restart(
            &engine,
            id.as_str(),
            shipyard_engine::run::StopOptions::default(),
        )
        .unwrap();
        assert!(matches!(stores.state(&id), ContainerState::Running { .. }));
        eventually("its second start", || {
            std::fs::read_to_string(&starts).is_ok_and(|text| text.lines().count() == 2)
        });
        let logs = containers::testing::logs_dir(&engine, &id).unwrap();
        assert_eq!(names(&logs).len(), 2, "two starts");
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// A `stop` killed as it waits changes nothing: the monitor sees the
    /// stop through, SIGKILL at the timeout.
    fn a_stop_killed_meanwhile_still_ends_it() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id = started_deaf(&stores, &engine);
        let mut stopping = stores
            .container_command("stop", id.as_str())
            .arg("2")
            .spawn()
            .unwrap();
        let termed = stores.upper(&id).join("termed");
        eventually("the stop's SIGTERM", || termed.exists());
        stopping.kill().unwrap();
        stopping.wait().unwrap();
        assert!(matches!(stores.state(&id), ContainerState::Running { .. }));
        eventually("its end", || stores.record(&id).exit.is_some());
        assert_eq!(exit_code(&stores, &id), Some(137));
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// Three stops (the runtime's: the engine's would wait on its operation
    /// lock): the earliest SIGKILL wins, however the stops come.
    fn the_earliest_stop_wins() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id = started_deaf(&stores, &engine);
        let root = stores.runtime_root();
        let term = shipyard_runtime::process::Signal::TERM;
        assert!(runtime::stop(&root, id.as_str(), term, Duration::from_secs(60)).unwrap());
        let began = Instant::now();
        assert!(runtime::stop(&root, id.as_str(), term, Duration::from_secs(1)).unwrap());
        assert!(runtime::stop(&root, id.as_str(), term, Duration::from_secs(30)).unwrap());
        eventually("its end", || stores.record(&id).exit.is_some());
        assert!(
            began.elapsed() < Duration::from_secs(10),
            "{:?}",
            began.elapsed()
        );
        assert_eq!(exit_code(&stores, &id), Some(137));
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// Only a control client may stop it: an attach client saying `Stop`
    /// isn't the protocol, and is let go; so is a waiter saying it; the
    /// container runs on. A control client of version 1 (there were none)
    /// is refused.
    fn an_attach_client_cant_stop_it() {
        use shipyard_runtime::attach::{Decoder, Frame, VERSION};
        use std::io::{Read as _, Write as _};
        let stores = Stores::new();
        let engine = stores.engine();
        let id = started_deaf(&stores, &engine);
        let mut stream = runtime::attach_stream(&stores.runtime_root(), id.as_str()).unwrap();
        let mut bytes = Vec::new();
        Frame::Hello {
            version: VERSION,
            detach_keys: Vec::new(),
        }
        .encode(&mut bytes);
        Frame::Stop {
            signal: 9,
            timeout_secs: 0,
        }
        .encode(&mut bytes);
        stream.write_all(&bytes).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        // Welcomed, then let go: the stream ends.
        let mut decoder = Decoder::new();
        let mut buffer = [0; 4096];
        let mut welcomed = false;
        loop {
            while let Ok(Some(frame)) = decoder.next_frame() {
                welcomed |= matches!(frame, Frame::Welcome { .. });
            }
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => decoder.push(&buffer[..read]),
                Err(error) => panic!("not let go: {error}"),
            }
        }
        assert!(welcomed);
        let mut old = runtime::attach_stream(&stores.runtime_root(), id.as_str()).unwrap();
        let mut hello = Vec::new();
        Frame::HelloControl {
            version: 1,
            stopper: false,
        }
        .encode(&mut hello);
        old.write_all(&hello).unwrap();
        old.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut decoder = Decoder::new();
        let refused = loop {
            if let Ok(Some(frame)) = decoder.next_frame() {
                break frame;
            }
            let read = old.read(&mut buffer).unwrap();
            assert_ne!(read, 0, "closed without a word");
            decoder.push(&buffer[..read]);
        };
        assert!(matches!(refused, Frame::Refused(_)), "{refused:?}");
        // A waiter saying `Stop` isn't the protocol either.
        let mut waiter = runtime::attach_stream(&stores.runtime_root(), id.as_str()).unwrap();
        let mut bytes = Vec::new();
        Frame::HelloControl {
            version: VERSION,
            stopper: false,
        }
        .encode(&mut bytes);
        Frame::Stop {
            signal: 9,
            timeout_secs: 0,
        }
        .encode(&mut bytes);
        waiter.write_all(&bytes).unwrap();
        waiter
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        while waiter.read(&mut buffer).is_ok_and(|read| read > 0) {}
        std::thread::sleep(Duration::from_millis(500));
        assert!(
            matches!(stores.state(&id), ContainerState::Running { .. }),
            "{:?}",
            stores.state(&id)
        );
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// A control client is let in, sent no output (the container prints all
    /// the while), and told the end.
    fn a_control_client_sees_no_output() {
        use shipyard_runtime::attach::{Decoder, Frame, VERSION};
        use std::io::{Read as _, Write as _};
        let stores = Stores::new();
        let engine = stores.engine();
        let id = shipyard_engine::run::create(
            &engine,
            stores.request(&["sh", "-c", "while :; do echo chatter; sleep 0.1; done"]),
            false,
        )
        .unwrap()
        .id;
        shipyard_engine::run::start(&engine, id.as_str()).unwrap();
        let mut stream = runtime::attach_stream(&stores.runtime_root(), id.as_str()).unwrap();
        let mut hello = Vec::new();
        Frame::HelloControl {
            version: VERSION,
            stopper: false,
        }
        .encode(&mut hello);
        stream.write_all(&hello).unwrap();
        let reader = std::thread::spawn(move || {
            let mut decoder = Decoder::new();
            let mut buffer = [0; 4096];
            let mut frames = Vec::new();
            loop {
                while let Ok(Some(frame)) = decoder.next_frame() {
                    frames.push(frame);
                }
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => return frames,
                    Ok(read) => decoder.push(&buffer[..read]),
                }
            }
        });
        std::thread::sleep(Duration::from_secs(1));
        stores.kill_and_wait(&id);
        let frames = reader.join().unwrap();
        assert!(
            matches!(
                frames.as_slice(),
                [Frame::Welcome { .. }, Frame::Exited(137)]
            ),
            "{frames:?}"
        );
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// A real-time stop signal (systemd images' `SIGRTMIN+3`) reaches the
    /// command: it ends on it, long before the SIGKILL.
    fn a_real_time_stop_signal_arrives() {
        let stores = Stores::new();
        let engine = stores.engine();
        let signal: shipyard_runtime::process::Signal = "SIGRTMIN+3".parse().unwrap();
        let script = format!(
            "trap 'exit 3' {}; touch /ready; while :; do sleep 1; done",
            signal.as_raw()
        );
        let id =
            shipyard_engine::run::create(&engine, stores.request(&["sh", "-c", &script]), false)
                .unwrap()
                .id;
        shipyard_engine::run::start(&engine, id.as_str()).unwrap();
        let ready = stores.upper(&id).join("ready");
        eventually("its trap set", || ready.exists());
        let options = shipyard_engine::run::StopOptions {
            signal: Some(signal),
            timeout: Some(Duration::from_secs(30)),
        };
        let began = Instant::now();
        shipyard_engine::run::stop(&engine, id.as_str(), options).unwrap();
        assert!(
            began.elapsed() < Duration::from_secs(10),
            "{:?}",
            began.elapsed()
        );
        assert_eq!(exit_code(&stores, &id), Some(3));
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// A `stop` with a long timeout holds the operation lock while it
    /// waits; a shorter `stop` (or `rm -f`) asks the monitor before waiting
    /// for that lock, so its SIGKILL comes on its own time, not the first's.
    fn a_shorter_stop_goes_past_a_longer_one() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id = started_deaf(&stores, &engine);
        let long = stores
            .container_command("stop", id.as_str())
            .arg("300")
            .spawn()
            .unwrap();
        let termed = stores.upper(&id).join("termed");
        eventually("the long stop's SIGTERM", || termed.exists());
        let began = Instant::now();
        let options = shipyard_engine::run::StopOptions {
            timeout: Some(Duration::from_secs(1)),
            ..Default::default()
        };
        shipyard_engine::run::stop(&engine, id.as_str(), options).unwrap();
        assert!(
            began.elapsed() < Duration::from_secs(10),
            "{:?}",
            began.elapsed()
        );
        assert_eq!(exit_code(&stores, &id), Some(137));
        let long = long.wait_with_output().unwrap();
        assert!(long.status.success(), "{long:?}");
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// As many waiters as the monitor lets in (each `wait` holds a place
    /// until the end) never keep a `stop` out: stoppers have places of
    /// their own. Every waiter is told the end; an engine `wait` turned
    /// away (one too many) waits for the settled end instead.
    fn waiters_never_keep_a_stop_out() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id = shipyard_engine::run::create(&engine, stores.request(&["sleep", "300"]), false)
            .unwrap()
            .id;
        shipyard_engine::run::start(&engine, id.as_str()).unwrap();
        let root = stores.runtime_root();
        // The monitor's sockets: each waiter it lets in is one more, held
        // until the end.
        let monitor = parent(runtime::state(&root, id.as_str()).unwrap().pid.unwrap());
        let before = sockets(monitor);
        let waiters: Vec<_> = (0..16)
            .map(|_| {
                let (root, id) = (root.clone(), id.clone());
                std::thread::spawn(move || runtime::wait(&root, id.as_str()))
            })
            .collect();
        eventually("the monitor holding the 16 waiters", || {
            sockets(monitor) >= before + 16
        });
        let one_too_many = {
            let (engine, id) = (stores.engine(), id.clone());
            std::thread::spawn(move || shipyard_engine::run::wait(&engine, id.as_str()))
        };
        // A head start for the one turned away (nothing outside it shows
        // it was): if the stop overtakes it, it reads the settled end all
        // the same, which is asserted below either way.
        std::thread::sleep(Duration::from_millis(500));
        shipyard_engine::run::stop(
            &engine,
            id.as_str(),
            shipyard_engine::run::StopOptions::default(),
        )
        .unwrap();
        for waiter in waiters {
            assert_eq!(waiter.join().unwrap().unwrap(), Some(143));
        }
        assert_eq!(one_too_many.join().unwrap().unwrap(), 143);
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// The lines `logs` gives for container `reference`, as `options` say.
    fn logged(
        engine: &shipyard_engine::Engine,
        reference: &str,
        options: &shipyard_engine::logs::LogsOptions,
    ) -> Vec<String> {
        let mut lines = Vec::new();
        shipyard_engine::logs::of_container(engine, reference, options, |entry| {
            lines.push(String::from_utf8_lossy(&entry.text).into_owned());
            Ok(())
        })
        .unwrap();
        lines
    }

    /// A command that counts its starts (`/n`) and writes two lines for
    /// each: `line-N-a`, `line-N-b`.
    const COUNTING: &[&str] = &[
        "sh",
        "-c",
        "n=$(cat /n 2>/dev/null || echo 0); n=$((n + 1)); echo $n > /n; echo line-$n-a; echo \
         line-$n-b",
    ];

    /// `logs`: every start's, oldest first; `--tail` across starts.
    fn logs_cover_every_start() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id = shipyard_engine::run::create(&engine, stores.request(COUNTING), false)
            .unwrap()
            .id;
        for _ in 0..2 {
            shipyard_engine::run::start(&engine, id.as_str()).unwrap();
            assert_eq!(shipyard_engine::run::wait(&engine, id.as_str()).unwrap(), 0);
            eventually("its end settled", || {
                stores.running_file(&id).try_lock().is_ok()
            });
        }
        let options = |tail| shipyard_engine::logs::LogsOptions {
            tail,
            ..Default::default()
        };
        assert_eq!(
            logged(&engine, id.as_str(), &options(None)),
            ["line-1-a", "line-1-b", "line-2-a", "line-2-b"]
        );
        assert_eq!(
            logged(&engine, id.as_str(), &options(Some(3))),
            ["line-1-b", "line-2-a", "line-2-b"]
        );
        assert_eq!(
            logged(&engine, id.as_str(), &options(Some(1))),
            ["line-2-b"]
        );
        // A start that never wrote its logs (it failed early), and what
        // isn't a start: left out, followed or not (it isn't up).
        let logs = shipyard_engine::containers::testing::logs_dir(&engine, &id).unwrap();
        std::fs::create_dir(logs.join("0000000003")).unwrap();
        std::fs::create_dir(logs.join("notes")).unwrap();
        std::fs::write(logs.join("0000000004"), "").unwrap();
        for follow in [false, true] {
            for (tail, expected) in [
                (None, &["line-1-a", "line-1-b", "line-2-a", "line-2-b"][..]),
                (Some(3), &["line-1-b", "line-2-a", "line-2-b"][..]),
            ] {
                let options = shipyard_engine::logs::LogsOptions {
                    tail,
                    follow,
                    ..Default::default()
                };
                assert_eq!(
                    logged(&engine, id.as_str(), &options),
                    expected,
                    "{tail:?}, following: {follow}"
                );
            }
        }
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// `logs -f`: what's there, then what comes, until the container ends
    /// (with the earlier start's first).
    fn logs_follow_until_it_ends() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id = shipyard_engine::run::create(
            &engine,
            stores.request(&["sh", "-c", "echo before; sleep 1; echo after"]),
            false,
        )
        .unwrap()
        .id;
        shipyard_engine::run::start(&engine, id.as_str()).unwrap();
        shipyard_engine::run::wait(&engine, id.as_str()).unwrap();
        eventually("its end settled", || {
            stores.running_file(&id).try_lock().is_ok()
        });
        shipyard_engine::run::start(&engine, id.as_str()).unwrap();
        let began = Instant::now();
        let options = shipyard_engine::logs::LogsOptions {
            follow: true,
            ..Default::default()
        };
        let lines = logged(&engine, id.as_str(), &options);
        assert_eq!(lines, ["before", "after", "before", "after"]);
        assert!(
            began.elapsed() < Duration::from_secs(10),
            "{:?}",
            began.elapsed()
        );
        // `-f --tail 1`: the last line there, across starts (the earlier
        // start's, if the newest has written nothing yet), then what comes.
        eventually("its end settled", || {
            stores.running_file(&id).try_lock().is_ok()
        });
        shipyard_engine::run::start(&engine, id.as_str()).unwrap();
        let lines = logged(
            &engine,
            id.as_str(),
            &shipyard_engine::logs::LogsOptions {
                tail: Some(1),
                ..options
            },
        );
        assert!(
            lines == ["after", "before", "after"] || lines == ["before", "after"],
            "{lines:?}"
        );
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// `logs -f` ends once the container stops (Docker's), and never goes
    /// on into its next start.
    fn logs_follow_ends_when_it_stops() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id = shipyard_engine::run::create(
            &engine,
            stores.request(&["sh", "-c", "echo up; exec sleep 300"]),
            false,
        )
        .unwrap()
        .id;
        shipyard_engine::run::start(&engine, id.as_str()).unwrap();
        eventually("it wrote", || {
            logged(
                &engine,
                id.as_str(),
                &shipyard_engine::logs::LogsOptions::default(),
            ) == ["up"]
        });
        let follower = {
            let engine = stores.engine();
            let id = id.clone();
            std::thread::spawn(move || {
                let options = shipyard_engine::logs::LogsOptions {
                    follow: true,
                    ..Default::default()
                };
                logged(&engine, id.as_str(), &options)
            })
        };
        std::thread::sleep(Duration::from_millis(500));
        assert!(!follower.is_finished(), "it follows while it runs");
        let stopped = Instant::now();
        shipyard_engine::run::stop(
            &engine,
            id.as_str(),
            shipyard_engine::run::StopOptions {
                timeout: Some(Duration::from_secs(1)),
                ..Default::default()
            },
        )
        .unwrap();
        shipyard_engine::run::start(&engine, id.as_str()).unwrap();
        eventually("the follower ended", || follower.is_finished());
        assert_eq!(follower.join().unwrap(), ["up"]);
        assert!(
            stopped.elapsed() < Duration::from_secs(15),
            "{:?}",
            stopped.elapsed()
        );
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// `attach`: the container's output, and its exit code at its end; one
    /// that isn't up can't be attached to.
    fn attach_gets_its_output_and_its_end() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id = shipyard_engine::run::create(
            &engine,
            stores.request(&["sh", "-c", "sleep 1; echo hello; exit 4"]),
            false,
        )
        .unwrap()
        .id;
        shipyard_engine::run::start(&engine, id.as_str()).unwrap();
        let attached = stores
            .container_command("attach", id.as_str())
            .output()
            .unwrap();
        let out = String::from_utf8_lossy(&attached.stdout);
        assert!(
            out.contains("hello") && out.contains("code=Exited(4)"),
            "{attached:?}"
        );
        let refused =
            shipyard_engine::attach::to_container(&engine, id.as_str(), &quiet()).unwrap_err();
        assert_eq!(
            refused.kind(),
            shipyard_engine::ErrorKind::NotRunning,
            "{refused:#}"
        );
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// `start -a`: let in before the start gate opens (no warning that it
    /// wasn't), so even the first thing the command writes, at once, comes;
    /// then its exit code.
    fn start_a_misses_nothing() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id = shipyard_engine::run::create(
            &engine,
            stores.request(&["sh", "-c", "echo first; exit 3"]),
            false,
        )
        .unwrap()
        .id;
        for _ in 0..3 {
            let started = stores
                .container_command("start-a", id.as_str())
                .output()
                .unwrap();
            let out = String::from_utf8_lossy(&started.stdout);
            assert!(
                out.contains("first") && out.contains("code=Exited(3)"),
                "{started:?}"
            );
            // Let in before the start, not after it (which mostly sees
            // everything too: a connect is quicker than a command's start,
            // so only the warning tells them apart).
            assert!(
                !String::from_utf8_lossy(&started.stderr).contains("before it started"),
                "{started:?}"
            );
            eventually("its end settled", || {
                stores.running_file(&id).try_lock().is_ok()
            });
        }
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// `exec -u NAME`: that user's IDs and groups, from the container's own
    /// `/etc/passwd` and `/etc/group`, and its home as `HOME`.
    fn exec_runs_as_a_user_by_name() {
        let stores = Stores::new();
        let engine = stores.engine();
        let root = shipyard_test_support::rootfs::RootFs::minimal();
        std::fs::create_dir_all(root.path().join("etc")).unwrap();
        std::fs::write(
            root.path().join("etc/passwd"),
            "root:x:0:0:root:/root:/bin/sh\nbuilder:x:1000:1001::/home/builder:/bin/sh\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("etc/group"),
            "root:x:0:\nbuilders:x:1001:\nstaff:x:50:builder\n",
        )
        .unwrap();
        let (_registry, image) = shipyard_test_support::registry::fixture_of(&root, 0);
        let mut request = stores.request(&["sleep", "300"]);
        request.image = Some(image.clone());
        let id = shipyard_engine::run::create(&engine, request, false)
            .unwrap()
            .id;
        shipyard_engine::run::start(&engine, id.as_str()).unwrap();
        let ran = stores
            .container_command("exec", id.as_str())
            .args([
                "builder",
                "sh",
                "-c",
                "echo home=$HOME; cat /proc/self/status",
            ])
            .output()
            .unwrap();
        let out = String::from_utf8_lossy(&ran.stdout);
        let field = |name: &str| {
            out.lines()
                .find_map(|line| line.strip_prefix(name))
                .map(|value| value.split_whitespace().collect::<Vec<_>>().join(" "))
                .unwrap_or_default()
        };
        assert!(out.contains("home=/home/builder"), "{ran:?}");
        assert_eq!(field("Uid:"), "1000 1000 1000 1000", "{ran:?}");
        assert_eq!(field("Gid:"), "1001 1001 1001 1001", "{ran:?}");
        assert_eq!(field("Groups:"), "50", "{ran:?}");
        assert!(out.contains("ran=Exited(0)"), "{ran:?}");
        // Numbers: those IDs, as given.
        let ran = stores
            .container_command("exec", id.as_str())
            .args(["1000:50", "cat", "/proc/self/status"])
            .output()
            .unwrap();
        let out = String::from_utf8_lossy(&ran.stdout);
        assert!(out.contains("Uid:\t1000\t1000"), "{ran:?}");
        assert!(out.contains("Gid:\t50\t50"), "{ran:?}");
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
        // A container with a `HOME` of its own and a group added: an exec
        // as another user keeps both (Docker's).
        let mut request = stores.request(&["sleep", "300"]);
        request.image = Some(image);
        request.env.push("HOME=/given".into());
        request.group_add.push("7".to_owned());
        let id = shipyard_engine::run::create(&engine, request, false)
            .unwrap()
            .id;
        shipyard_engine::run::start(&engine, id.as_str()).unwrap();
        let ran = stores
            .container_command("exec", id.as_str())
            .args([
                "builder",
                "sh",
                "-c",
                "echo home=$HOME; cat /proc/self/status",
            ])
            .output()
            .unwrap();
        let out = String::from_utf8_lossy(&ran.stdout);
        let field = |name: &str| {
            out.lines()
                .find_map(|line| line.strip_prefix(name))
                .map(|value| value.split_whitespace().collect::<Vec<_>>().join(" "))
                .unwrap_or_default()
        };
        assert!(out.contains("home=/given"), "{ran:?}");
        assert_eq!(field("Groups:"), "7 50", "{ran:?}");
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// Without `-i`, the command's input is `/dev/null`: it reads
    /// end-of-file at once, though this side's input stays open.
    fn exec_without_input_reads_nothing() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id = shipyard_engine::run::create(&engine, stores.request(&["sleep", "300"]), false)
            .unwrap()
            .id;
        shipyard_engine::run::start(&engine, id.as_str()).unwrap();
        let mut child = stores
            .container_command("exec", id.as_str())
            .args(["-", "sh", "-c", "cat; echo read-all"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        // Held open: a command reading it would wait forever.
        let _input = child.stdin.take();
        eventually("it ended", || child.try_wait().unwrap().is_some());
        let mut out = String::new();
        std::io::Read::read_to_string(child.stdout.as_mut().unwrap(), &mut out).unwrap();
        assert!(out.contains("read-all"), "{out}");
        // With `-i`, it reads what's typed, to its end.
        let mut child = stores
            .container_command("exec-i", id.as_str())
            .args(["-", "sh", "-c", "cat; echo read-all"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        std::io::Write::write_all(&mut child.stdin.take().unwrap(), b"typed\n").unwrap();
        let ran = child.wait_with_output().unwrap();
        let out = String::from_utf8_lossy(&ran.stdout);
        assert!(out.contains("typed\nread-all"), "{ran:?}");
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// A command that isn't there: exit code 127; one that is and can't be
    /// run, 126 (Docker's).
    fn exec_of_a_missing_command_is_127() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id = shipyard_engine::run::create(&engine, stores.request(&["sleep", "300"]), false)
            .unwrap()
            .id;
        shipyard_engine::run::start(&engine, id.as_str()).unwrap();
        let error = shipyard_engine::exec::in_container(
            &engine,
            id.as_str(),
            &shipyard_engine::exec::ExecRequest {
                args: vec!["/no/such/command".into()],
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_eq!(shipyard_engine::exec::exit_code(&error), 127, "{error:#}");
        // There, and not executable: 126.
        let error = shipyard_engine::exec::in_container(
            &engine,
            id.as_str(),
            &shipyard_engine::exec::ExecRequest {
                args: vec!["/marker".into()],
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_eq!(shipyard_engine::exec::exit_code(&error), 126, "{error:#}");
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// `cp FROM TO` as the CLI reads its operands, by name, its tar streams
    /// none (`-` reads nothing, writes nowhere).
    fn cp(
        engine: &shipyard_engine::Engine,
        from: &str,
        to: &str,
    ) -> shipyard_engine::Result<shipyard_engine::cp::CpReport> {
        cp_with(engine, from, to, &mut std::io::empty(), false)
    }

    /// [`cp`], a copy in's stream read from `input`, `-a` if `archive`.
    fn cp_with(
        engine: &shipyard_engine::Engine,
        from: &str,
        to: &str,
        input: &mut dyn std::io::Read,
        archive: bool,
    ) -> shipyard_engine::Result<shipyard_engine::cp::CpReport> {
        use shipyard_engine::cp::{CpOptions, CpStreams, Operand, Transfer, copy_by_name};
        let transfer = Transfer::of(
            Operand::parse(std::ffi::OsStr::new(from)),
            Operand::parse(std::ffi::OsStr::new(to)),
        )
        .unwrap();
        let options = CpOptions {
            archive,
            ..CpOptions::default()
        };
        let mut streams = CpStreams {
            input,
            output: &mut std::io::sink(),
        };
        copy_by_name(engine, &transfer, &mut streams, options)
    }

    /// `cp` by name: a file out of the container, and one in.
    fn cp_copies_by_name() {
        let stores = Stores::new();
        let engine = stores.engine();
        let record =
            shipyard_engine::run::create(&engine, stores.request(&["sleep", "300"]), false)
                .unwrap();
        shipyard_engine::run::start(&engine, record.id.as_str()).unwrap();
        let host = stores.scratch.0.join("copied");
        let host = host.to_str().unwrap();
        cp(&engine, &format!("{}:/marker", record.name), host).unwrap();
        assert_eq!(
            std::fs::read_to_string(host).unwrap(),
            shipyard_test_support::rootfs::MARKER
        );
        std::fs::write(host, "in\n").unwrap();
        cp(&engine, host, &format!("{}:/copied-in", record.name)).unwrap();
        assert_eq!(
            std::fs::read_to_string(stores.upper(&record.id).join("copied-in")).unwrap(),
            "in\n"
        );
        shipyard_engine::run::remove(&engine, record.id.as_str(), true).unwrap();
    }

    /// This process's mounts, as its mount namespace has them.
    fn own_mounts() -> String {
        std::fs::read_to_string("/proc/self/mountinfo").unwrap()
    }

    /// `cp` of a stopped container: what it wrote, out; a file in, which its
    /// next start sees. Its root mounted only in the helper's own mount
    /// namespace: nothing is left mounted here.
    fn cp_reaches_a_stopped_container() {
        let stores = Stores::new();
        let engine = stores.engine();
        let record = shipyard_engine::run::create(
            &engine,
            stores.request(&["sh", "-c", "echo made > /made; cat /in 2>/dev/null; true"]),
            false,
        )
        .unwrap();
        let name = record.name.as_str();
        shipyard_engine::run::start(&engine, name).unwrap();
        shipyard_engine::run::wait(&engine, name).unwrap();
        eventually("its end settled", || {
            stores.running_file(&record.id).try_lock().is_ok()
        });
        let mounts = own_mounts();
        let host = stores.scratch.0.join("made");
        cp(&engine, &format!("{name}:/made"), host.to_str().unwrap()).unwrap();
        assert_eq!(std::fs::read_to_string(&host).unwrap(), "made\n");
        std::fs::write(&host, "copied in\n").unwrap();
        cp(&engine, host.to_str().unwrap(), &format!("{name}:/in")).unwrap();
        assert_eq!(own_mounts(), mounts, "nothing left mounted");
        shipyard_engine::run::start(&engine, name).unwrap();
        shipyard_engine::run::wait(&engine, name).unwrap();
        assert!(
            logged(
                &engine,
                name,
                &shipyard_engine::logs::LogsOptions::default()
            )
            .contains(&"copied in".to_owned())
        );
        shipyard_engine::run::remove(&engine, name, true).unwrap();
    }

    /// A start that fails after it rewrote the record (here its runtime
    /// start, broken) puts back what it rewrote (F55): here its user's stop,
    /// the restart count and backoff, and its last end (the next start and
    /// the ports are the unit's, `a_previous_start_puts_back_all_a_start_rewrote`);
    /// so its `unless-stopped` policy doesn't bring back what its user
    /// stopped. (`between` runs after the record says what the start
    /// rewrites, before the runtime starts it.)
    fn a_failed_start_keeps_what_its_user_stopped() {
        let stores = Stores::new();
        let engine = stores.engine();
        let mut request = stores.request(&["sleep", "300"]);
        request.restart = shipyard_engine::containers::RestartPolicy::UnlessStopped;
        let id = shipyard_engine::run::create(&engine, request, false)
            .unwrap()
            .id;
        shipyard_engine::run::start(&engine, id.as_str()).unwrap();
        let fast = shipyard_engine::run::StopOptions {
            timeout: Some(Duration::ZERO),
            ..Default::default()
        };
        shipyard_engine::run::stop(&engine, id.as_str(), fast).unwrap();
        eventually("its end settled", || {
            stores.running_file(&id).try_lock().is_ok()
        });
        containers::testing::update(&engine, &id, |record| {
            record.restart_count = 2;
            record.restart_delay_ms = 400;
        })
        .unwrap();
        let before = stores.record(&id);
        assert!(
            before.stopped_by_user && before.finished.is_some(),
            "{before:?}"
        );
        let root = stores.runtime_root();
        let failed = shipyard_engine::run::start_with(&engine, id.as_str(), |id| {
            std::fs::remove_file(root.join(id.as_str()).join("exec.fifo")).unwrap();
        });
        assert!(failed.is_err(), "{failed:?}");
        eventually("its clean-up done", || {
            stores.running_file(&id).try_lock().is_ok()
                && runtime::status(&stores.runtime_root(), id.as_str())
                    .unwrap()
                    .is_none()
        });
        let after = stores.record(&id);
        let kept = |record: &containers::Record| {
            (
                record.stopped_by_user,
                record.restart_count,
                record.restart_delay_ms,
                record.next_start,
                record.ports.clone(),
                record.started,
                record.finished,
                record.exit.clone(),
            )
        };
        assert_eq!(kept(&after), kept(&before));
        assert!(
            matches!(stores.state(&id), ContainerState::Exited { .. }),
            "not brought back: {:?}",
            stores.state(&id)
        );
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// A `stop` of a container whose start died after it recorded itself
    /// (its CLI killed: faked as `a_start_killed_after_recording_itself_keeps_its_history`
    /// does, with a v2 `previous-start.json`) stays a stop: the dead start's
    /// undo puts back its last end but not "not stopped by its user"
    /// (10.2b1's Murphy review), so its `unless-stopped` policy doesn't
    /// bring it back.
    fn a_stop_of_a_dead_start_stays_stopped() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id = Stores::id(&stores.launch(&[
            "--network",
            "none",
            "--restart",
            "unless-stopped",
            stores.image.as_str(),
            "sleep",
            "300",
        ]));
        // Stopped by its user (its policy leaves it so), its end settled;
        // then the mark cleared, as a start does before it dies.
        let fast = shipyard_engine::run::StopOptions {
            timeout: Some(Duration::ZERO),
            ..Default::default()
        };
        shipyard_engine::run::stop(&engine, id.as_str(), fast).unwrap();
        eventually("its first end, settled", || {
            matches!(stores.state(&id), ContainerState::Exited { .. })
                && stores.running_file(&id).try_lock().is_ok()
        });
        containers::testing::update(&engine, &id, |record| record.stopped_by_user = false).unwrap();
        let first = stores.record(&id);
        let killed = stores
            .container_command("start-abort", id.as_str())
            .output()
            .unwrap();
        assert!(!killed.status.success(), "{killed:?}");
        let kept = serde_json::json!({
            "version": 2,
            "started": first.started,
            "finished": first.finished,
            "exit": first.exit,
            "boot_id": first.boot_id,
            "rewritten": {
                "stopped_by_user": false,
                "restart_count": first.restart_count,
                "restart_delay_ms": first.restart_delay_ms,
                "next_start": null,
                "ports": [],
            },
        });
        let dir = containers::testing::record_dir(&engine, &id).unwrap();
        std::fs::write(
            dir.join("previous-start.json"),
            serde_json::to_vec(&kept).unwrap(),
        )
        .unwrap();
        containers::testing::update(&engine, &id, |record| {
            record.started = Some(SystemTime::now());
            record.finished = None;
            record.exit = None;
        })
        .unwrap();
        shipyard_engine::run::stop(&engine, id.as_str(), fast).unwrap();
        eventually("its clean-up done", || {
            stores.running_file(&id).try_lock().is_ok()
        });
        let record = stores.record(&id);
        assert!(record.stopped_by_user, "the stop kept: {record:?}");
        assert_eq!(record.next_start, None);
        assert_eq!(record.finished, first.finished, "its last end back");
        // What its policy reads (`restart_policy::after_end`): stopped by
        // its user, so no restart (no sleep then a negative check: AGENTS §9).
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// A container's writable layer is mounted once at a time (F60): its
    /// area's exclusive lock, which a start and a stopped container's `cp`
    /// view both take (`image::prepare_owned`), is the guarantee (overlay
    /// is mounted with `index=off`, so the kernel's own refusal is off).
    /// Held by another, a view and a start are each refused as in use;
    /// let go, both work (the positive control).
    fn a_held_area_refuses_a_view_and_a_start() {
        let stores = Stores::new();
        let engine = stores.engine();
        let record =
            shipyard_engine::run::create(&engine, stores.request(&["true"]), false).unwrap();
        let name = record.name.as_str();
        shipyard_engine::run::start(&engine, name).unwrap();
        shipyard_engine::run::wait(&engine, name).unwrap();
        eventually("its end settled", || {
            stores.running_file(&record.id).try_lock().is_ok()
        });
        let area = stores.upper(&record.id).parent().unwrap().to_owned();
        let holder = std::fs::File::open(&area).unwrap();
        holder.try_lock().unwrap();
        let host = stores.scratch.0.join("marker");
        let host = host.to_str().unwrap();
        let viewed = cp(&engine, &format!("{name}:/marker"), host);
        assert_eq!(
            viewed.as_ref().map_err(shipyard_engine::Error::kind).err(),
            Some(shipyard_engine::ErrorKind::InUse),
            "a view while the area is held: {viewed:?}"
        );
        let started = shipyard_engine::run::start(&engine, name);
        assert_eq!(
            started.as_ref().map_err(shipyard_engine::Error::kind).err(),
            Some(shipyard_engine::ErrorKind::InUse),
            "a start while the area is held: {started:?}"
        );
        drop(holder);
        cp(&engine, &format!("{name}:/marker"), host).unwrap();
        shipyard_engine::run::start(&engine, name).unwrap();
        shipyard_engine::run::wait(&engine, name).unwrap();
        shipyard_engine::run::remove(&engine, name, true).unwrap();
    }

    /// A container made and never started: a copy in makes its writable
    /// layer (and its volumes), and its first start sees the files.
    fn cp_into_a_created_container() {
        let stores = Stores::new();
        let engine = stores.engine();
        let mut request = stores.request(&["cat", "/in", "/data/in"]);
        request.volume = vec!["fresh:/data".to_owned()];
        let record = shipyard_engine::run::create(&engine, request, false).unwrap();
        let name = record.name.as_str();
        let host = stores.scratch.0.join("in");
        std::fs::write(&host, "before its start\n").unwrap();
        cp(&engine, host.to_str().unwrap(), &format!("{name}:/in")).unwrap();
        // Its volume, never made yet (its first start would make it): made
        // by the copy, as Docker's `create` makes it.
        cp(&engine, host.to_str().unwrap(), &format!("{name}:/data/in")).unwrap();
        shipyard_engine::run::start(&engine, name).unwrap();
        assert_eq!(shipyard_engine::run::wait(&engine, name).unwrap(), 0);
        assert_eq!(
            logged(
                &engine,
                name,
                &shipyard_engine::logs::LogsOptions::default()
            ),
            ["before its start", "before its start"]
        );
        shipyard_engine::run::remove(&engine, name, true).unwrap();
    }

    /// A stopped container's volumes are mounted for a copy (Docker's): a
    /// named volume's file out, one in; a read-only bind refuses a copy in;
    /// a volume removed since is said, never made again.
    fn cp_reaches_a_stopped_containers_volumes() {
        let stores = Stores::new();
        let engine = stores.engine();
        let bound = stores.scratch.0.join("bound");
        std::fs::create_dir(&bound).unwrap();
        std::fs::write(bound.join("given"), "from the host\n").unwrap();
        let mut request = stores.request(&["sh", "-c", "echo kept > /data/x"]);
        request.volume = vec![
            "data:/data".to_owned(),
            format!("{}:/given:ro", bound.display()),
        ];
        let record = shipyard_engine::run::create(&engine, request, false).unwrap();
        let name = record.name.as_str();
        shipyard_engine::run::start(&engine, name).unwrap();
        assert_eq!(shipyard_engine::run::wait(&engine, name).unwrap(), 0);
        eventually("its end settled", || {
            stores.running_file(&record.id).try_lock().is_ok()
        });
        let path = |file: &str| stores.scratch.0.join(file);
        cp(
            &engine,
            &format!("{name}:/data/x"),
            path("x").to_str().unwrap(),
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(path("x")).unwrap(), "kept\n");
        cp(
            &engine,
            &format!("{name}:/given/given"),
            path("given").to_str().unwrap(),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(path("given")).unwrap(),
            "from the host\n"
        );
        std::fs::write(path("y"), "into the volume\n").unwrap();
        cp(
            &engine,
            path("y").to_str().unwrap(),
            &format!("{name}:/data/y"),
        )
        .unwrap();
        let volume = stores.scratch.store().join("volumes/data/_data");
        assert_eq!(
            std::fs::read_to_string(volume.join("y")).unwrap(),
            "into the volume\n"
        );
        assert!(!stores.upper(&record.id).join("data/y").exists());
        let refused = cp(
            &engine,
            path("y").to_str().unwrap(),
            &format!("{name}:/given/y"),
        )
        .unwrap_err();
        assert!(read_only(&refused), "{refused:#}");
        assert!(!bound.join("y").exists());
        // Its volume removed since, by hand (`volume rm` refuses a volume a
        // container names, stopped or not): said, never made again empty.
        std::fs::remove_dir_all(volume.parent().unwrap()).unwrap();
        let gone = cp(
            &engine,
            &format!("{name}:/marker"),
            path("m").to_str().unwrap(),
        )
        .unwrap_err();
        assert_eq!(
            gone.kind(),
            shipyard_engine::ErrorKind::NotFound,
            "{gone:#}"
        );
        assert!(!volume.exists(), "the volume was made again");
        shipyard_engine::run::remove(&engine, name, true).unwrap();
    }

    /// A stopped `--read-only` container: copies out, never in (as while it
    /// runs).
    fn cp_of_a_read_only_container_copies_out_only() {
        let stores = Stores::new();
        let engine = stores.engine();
        let mut request = stores.request(&["cat", "/marker"]);
        request.read_only = true;
        request.volume = vec!["data:/data".to_owned()];
        let record = shipyard_engine::run::create(&engine, request, false).unwrap();
        let name = record.name.as_str();
        shipyard_engine::run::start(&engine, name).unwrap();
        shipyard_engine::run::wait(&engine, name).unwrap();
        eventually("its end settled", || {
            stores.running_file(&record.id).try_lock().is_ok()
        });
        let host = stores.scratch.0.join("marker");
        cp(&engine, &format!("{name}:/marker"), host.to_str().unwrap()).unwrap();
        let refused = cp(&engine, host.to_str().unwrap(), &format!("{name}:/new")).unwrap_err();
        assert!(read_only(&refused), "{refused:#}");
        assert!(!stores.upper(&record.id).join("new").exists());
        // Its volume isn't the root: it takes a copy.
        cp(
            &engine,
            host.to_str().unwrap(),
            &format!("{name}:/data/new"),
        )
        .unwrap();
        shipyard_engine::run::remove(&engine, name, true).unwrap();
    }

    /// Whether `error` is the kernel's refusal to write a read-only file
    /// system.
    fn read_only(error: &shipyard_engine::Error) -> bool {
        let text = format!("{error:#}");
        text.contains("Read-only file system") || text.contains("EROFS")
    }

    /// A start that comes while a copy works on a stopped container waits
    /// for it (the operation lock), and sees what it copied.
    fn a_start_waits_for_a_copy() {
        let stores = Stores::new();
        let engine = stores.engine();
        let record =
            shipyard_engine::run::create(&engine, stores.request(&["cat", "/in"]), false).unwrap();
        let name = record.name.as_str();
        let host = stores.scratch.0.join("in");
        std::fs::write(&host, "copied first\n").unwrap();
        // The copy's helper waits 2 s once set up, before it copies.
        let mut copying = stores.container_command("cp", host.to_str().unwrap());
        copying
            .arg(format!("{name}:/in"))
            .env("SHIPYARD_TEST_HELPER_PAUSE", "2000");
        let mut copying = copying.spawn().unwrap();
        eventually("the copy holds the container", || {
            stores.worked_on(&record.id)
        });
        shipyard_engine::run::start(&engine, name).unwrap();
        // Started only once the copy was done (it had ended by then).
        assert!(
            copying.try_wait().unwrap().is_some(),
            "the start didn't wait for the copy"
        );
        let copied = copying.wait_with_output().unwrap();
        assert!(copied.status.success(), "{copied:?}");
        assert_eq!(shipyard_engine::run::wait(&engine, name).unwrap(), 0);
        assert_eq!(
            logged(
                &engine,
                name,
                &shipyard_engine::logs::LogsOptions::default()
            ),
            ["copied first"]
        );
        shipyard_engine::run::remove(&engine, name, true).unwrap();
    }

    /// A copy into a running container holds it too: should the container
    /// end meanwhile, a start waits for the copy, so its writable layer is
    /// never mounted again under the copy's helper (which outlives the
    /// container's init).
    fn a_start_waits_for_a_copy_into_one_that_ended() {
        let stores = Stores::new();
        let engine = stores.engine();
        let record =
            shipyard_engine::run::create(&engine, stores.request(&["sleep", "300"]), false)
                .unwrap();
        let name = record.name.as_str();
        shipyard_engine::run::start(&engine, name).unwrap();
        let host = stores.scratch.0.join("in");
        std::fs::write(&host, "copied while it ran\n").unwrap();
        let mut copying = stores.container_command("cp", host.to_str().unwrap());
        copying
            .arg(format!("{name}:/in"))
            .env("SHIPYARD_TEST_HELPER_PAUSE", "2000");
        let mut copying = copying.spawn().unwrap();
        eventually("the copy holds the container", || {
            stores.worked_on(&record.id)
        });
        // Ended under the copy (its init killed: no operation lock).
        let root = stores.runtime_root();
        shipyard_runtime::container::kill(
            &root,
            record.id.as_str(),
            shipyard_runtime::process::Signal::KILL,
        )
        .unwrap();
        eventually("its end settled", || {
            stores.running_file(&record.id).try_lock().is_ok()
        });
        shipyard_engine::run::start(&engine, name).unwrap();
        assert!(
            copying.try_wait().unwrap().is_some(),
            "the start didn't wait for the copy"
        );
        let copied = copying.wait_with_output().unwrap();
        assert!(copied.status.success(), "{copied:?}");
        let out = stores.scratch.0.join("out");
        cp(&engine, &format!("{name}:/in"), out.to_str().unwrap()).unwrap();
        assert_eq!(
            std::fs::read_to_string(&out).unwrap(),
            "copied while it ran\n"
        );
        shipyard_engine::run::remove(&engine, name, true).unwrap();
    }

    /// A copy as the container ends: its init gone but not yet reaped (its
    /// monitor held stopped here), so it's stopping. The copy waits for the
    /// end to settle (the monitor let go on again), then copies into it
    /// stopped. (The briefer window, an init still running whose namespaces
    /// are already gone, can't be held open here: a manual check met it.)
    fn cp_as_a_container_ends() {
        use nix::sys::signal::{Signal, kill};
        use nix::unistd::Pid;
        let stores = Stores::new();
        let engine = stores.engine();
        let record =
            shipyard_engine::run::create(&engine, stores.request(&["sh", "-c", "sleep 1"]), false)
                .unwrap();
        let name = record.name.as_str();
        shipyard_engine::run::start(&engine, name).unwrap();
        let root = stores.runtime_root();
        let (init, _) = shipyard_runtime::container::peek_init(&root, record.id.as_str())
            .unwrap()
            .unwrap();
        let stat =
            |pid: i32| std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        // The monitor is the init's parent (the fourth field).
        let monitor: i32 = stat(init)
            .rsplit(')')
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        kill(Pid::from_raw(monitor), Signal::SIGSTOP).unwrap();
        eventually("its init ended, unreaped", || {
            stat(init)
                .rsplit(')')
                .next()
                .is_some_and(|rest| rest.trim_start().starts_with('Z'))
        });
        let resume = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(1));
            kill(Pid::from_raw(monitor), Signal::SIGCONT).unwrap();
        });
        let host = stores.scratch.0.join("in");
        std::fs::write(&host, "late\n").unwrap();
        let copied = cp(&engine, host.to_str().unwrap(), &format!("{name}:/in"));
        resume.join().unwrap();
        copied.unwrap();
        assert_eq!(
            std::fs::read_to_string(stores.upper(&record.id).join("in")).unwrap(),
            "late\n"
        );
        shipyard_engine::run::remove(&engine, name, true).unwrap();
    }

    /// The only container in the test's store (a foreground run's).
    fn the_container(engine: &shipyard_engine::Engine) -> shipyard_engine::containers::Record {
        let mut listed = shipyard_engine::containers::list(engine).unwrap();
        assert_eq!(listed.len(), 1, "{listed:?}");
        listed.remove(0)
    }

    /// A foreground run is a container (Docker's): a record with a
    /// generated name, kept once it ends with its exit code and its logs;
    /// its output this process's from the first byte, stdout and stderr
    /// apart.
    fn a_foreground_run_is_a_container() {
        let stores = Stores::new();
        let engine = stores.engine();
        let ran = stores
            .foreground(&[], &["sh", "-c", "echo first; echo apart >&2; exit 3"])
            .output()
            .unwrap();
        assert_eq!(ran.status.code(), Some(3), "{ran:?}");
        assert_eq!(String::from_utf8_lossy(&ran.stdout), "first\n");
        let err = String::from_utf8_lossy(&ran.stderr);
        assert!(err.contains("apart\n") && err.contains("ran=3"), "{err}");
        let record = the_container(&engine);
        assert!(err.contains(&format!("name={}", record.name)), "{err}");
        // Recorded as ended by the time it returned.
        assert_eq!(record.exit.as_ref().and_then(|exit| exit.code), Some(3));
        assert_eq!(
            logged(
                &engine,
                record.id.as_str(),
                &shipyard_engine::logs::LogsOptions::default()
            ),
            ["first", "apart"]
        );
        shipyard_engine::run::remove(&engine, record.id.as_str(), true).unwrap();
    }

    /// `-t` (stage 9, step 9.4c3b): a container with a terminal of its own
    /// (its monitor's), its output this process's and its logs, though this
    /// process has no terminal.
    fn a_terminal_foreground_run_is_a_container() {
        let stores = Stores::new();
        let engine = stores.engine();
        let ran = stores
            .foreground(&["-t"], &["sh", "-c", "tty; echo first; exit 4"])
            .output()
            .unwrap();
        assert_eq!(ran.status.code(), Some(4), "{ran:?}");
        let out = String::from_utf8_lossy(&ran.stdout);
        // A terminal's line ends.
        assert!(out.contains("/dev/pts/0\r\nfirst\r\n"), "{out:?}");
        let record = the_container(&engine);
        assert!(record.config.tty);
        assert_eq!(record.exit.as_ref().and_then(|exit| exit.code), Some(4));
        let logs = logged(
            &engine,
            record.id.as_str(),
            &shipyard_engine::logs::LogsOptions::default(),
        );
        assert!(logs.iter().any(|line| line.contains("first")), "{logs:?}");
        shipyard_engine::run::remove(&engine, record.id.as_str(), true).unwrap();
    }

    /// `-it` with input that isn't a terminal is refused (Docker's words),
    /// before a container is made.
    fn a_terminal_foreground_run_needs_a_terminal_to_type_on() {
        let stores = Stores::new();
        let engine = stores.engine();
        let ran = stores
            .foreground(&["-i", "-t"], &["true"])
            .output()
            .unwrap();
        assert_eq!(ran.status.code(), Some(125), "{ran:?}");
        let err = String::from_utf8_lossy(&ran.stderr);
        assert!(err.contains("the input device is not a TTY"), "{err}");
        assert!(
            shipyard_engine::containers::list(&engine)
                .unwrap()
                .is_empty()
        );
    }

    /// `run --rm` returns only once the container is gone: no record, no
    /// writable layer left.
    fn a_foreground_run_with_rm_is_gone_when_it_returns() {
        let stores = Stores::new();
        let engine = stores.engine();
        let ran = stores
            .foreground(&["--rm"], &["sh", "-c", "echo bye"])
            .output()
            .unwrap();
        assert_eq!(ran.status.code(), Some(0), "{ran:?}");
        assert!(
            shipyard_engine::containers::list(&engine)
                .unwrap()
                .is_empty()
        );
        let areas = std::fs::read_dir(stores.scratch.store().join("containers"))
            .map_or(0, |entries| entries.flatten().count());
        assert_eq!(areas, 0, "its writable layer left");
    }

    /// `-i`: the caller's input is the container's, ending at its end of
    /// file (the run ends with it), binary bytes and all, and more than the
    /// monitor queues (the run waits for room: nothing is dropped); without
    /// it, `/dev/null`.
    fn input_ends_at_your_end_of_file() {
        let stores = Stores::new();
        let mut run = stores
            .foreground(&["--rm", "-i"], &["sh", "-c", "cat; echo eof"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        // Ctrl-P Ctrl-Q among it: input, never a detach (no keys without -it).
        std::io::Write::write_all(&mut run.stdin.take().unwrap(), b"a\x10\x11\nb\n").unwrap();
        let ran = run.wait_with_output().unwrap();
        assert_eq!(ran.status.code(), Some(0), "{ran:?}");
        assert_eq!(String::from_utf8_lossy(&ran.stdout), "a\x10\x11\nb\neof\n");
        let mut run = stores
            .foreground(&["--rm", "-i"], &["sh", "-c", "sleep 1; cat | wc -c"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = run.stdin.take().unwrap();
        let writer = std::thread::spawn(move || {
            std::io::Write::write_all(&mut input, &vec![b'x'; 1_000_000]).unwrap();
        });
        let ran = run.wait_with_output().unwrap();
        writer.join().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&ran.stdout).trim(),
            "1000000",
            "{ran:?}"
        );
        // Without -i: nothing to read, though this side's input stays open.
        let mut run = stores
            .foreground(&["--rm"], &["sh", "-c", "cat; echo none"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let _input = run.stdin.take();
        let ran = run.wait_with_output().unwrap();
        assert_eq!(String::from_utf8_lossy(&ran.stdout), "none\n", "{ran:?}");
    }

    /// Waits until the test's one container is running.
    fn running(engine: &shipyard_engine::Engine) {
        eventually("it runs", || {
            shipyard_engine::containers::list(engine)
                .unwrap()
                .first()
                .and_then(|record| shipyard_engine::containers::state(engine, record).ok())
                .is_some_and(|state| state.is_active())
        });
    }

    /// The signals the run gets are the container's (`--sig-proxy`): a
    /// TERM is its stop (its stop signal: 143, or as it traps it), an INT
    /// ends it with 130 and the run with SIGINT (a shell loop around it
    /// stops).
    fn a_forwarded_signal_ends_it_with_its_code() {
        use nix::sys::signal::{Signal, kill};
        let stores = Stores::new();
        let engine = stores.engine();
        for (signal, flags, command, ended) in [
            (Signal::SIGTERM, &[][..], "echo ready; sleep 30", Ok(143)),
            (
                Signal::SIGINT,
                &[][..],
                "echo ready; sleep 30",
                Err(libc::SIGINT),
            ),
            (
                Signal::SIGTERM,
                &[][..],
                "trap 'exit 3' TERM; echo ready; sleep 30 & wait",
                Ok(3),
            ),
            (
                Signal::SIGTERM,
                &["--stop-signal", "QUIT"][..],
                "trap 'exit 5' QUIT; echo ready; sleep 30 & wait",
                Ok(5),
            ),
        ] {
            let mut run = stores
                .foreground(&[&["--rm"][..], flags].concat(), &["sh", "-c", command])
                .spawn()
                .unwrap();
            // Its traps set (a start returns before its shell runs them).
            let mut ready = [0_u8; 6];
            std::io::Read::read_exact(run.stdout.as_mut().unwrap(), &mut ready).unwrap();
            assert_eq!(&ready, b"ready\n");
            kill(
                nix::unistd::Pid::from_raw(run.id().try_into().unwrap()),
                signal,
            )
            .unwrap();
            let ran = run.wait_with_output().unwrap();
            let how = match ran.status.code() {
                Some(code) => Ok(code),
                None => Err(std::os::unix::process::ExitStatusExt::signal(&ran.status).unwrap()),
            };
            assert_eq!(how, ended, "{signal} {flags:?}: {ran:?}");
            eventually("it's gone", || {
                shipyard_engine::containers::list(&engine)
                    .unwrap()
                    .is_empty()
            });
        }
    }

    /// Without `--sig-proxy` (or once the run is killed), the run's end is
    /// its own: the container runs on, as a detached one; with `--rm` it's
    /// removed when it ends.
    fn the_container_outlives_its_run() {
        use nix::sys::signal::{Signal, kill};
        let stores = Stores::new();
        let engine = stores.engine();
        for (flags, signal) in [
            (&["--no-sig-proxy"][..], Signal::SIGINT),
            (&["--rm"][..], Signal::SIGKILL),
        ] {
            let run = stores
                .foreground(flags, &["sh", "-c", "sleep 30"])
                .spawn()
                .unwrap();
            running(&engine);
            let record = the_container(&engine);
            let init = runtime::state(&stores.runtime_root(), record.id.as_str())
                .unwrap()
                .pid
                .unwrap();
            kill(
                nix::unistd::Pid::from_raw(run.id().try_into().unwrap()),
                signal,
            )
            .unwrap();
            let ran = run.wait_with_output().unwrap();
            assert_eq!(ran.status.code(), None, "{signal}: ended by it");
            // Not only up just after: the same init, still alive a while
            // later (a signal passed on, or an end following the run's,
            // would have ended it by then).
            std::thread::sleep(Duration::from_secs(1));
            assert!(
                shipyard_engine::containers::state(&engine, &record)
                    .unwrap()
                    .is_active(),
                "{signal}: the container ended with its run"
            );
            assert_eq!(
                runtime::state(&stores.runtime_root(), record.id.as_str())
                    .unwrap()
                    .pid,
                Some(init),
                "{signal}: the same init"
            );
            assert!(
                nix::sys::signal::kill(nix::unistd::Pid::from_raw(init), None).is_ok(),
                "{signal}: its init is gone"
            );
            shipyard_engine::run::stop(
                &engine,
                record.id.as_str(),
                shipyard_engine::run::StopOptions {
                    timeout: Some(Duration::from_secs(1)),
                    ..Default::default()
                },
            )
            .unwrap();
            if flags == ["--rm"] {
                eventually("removed at its end", || {
                    shipyard_engine::containers::list(&engine)
                        .unwrap()
                        .is_empty()
                });
            } else {
                shipyard_engine::run::remove(&engine, record.id.as_str(), true).unwrap();
            }
        }
    }

    /// A run that falls behind (stopped while the container writes more
    /// than its monitor holds for a client) is waited for, not let go of:
    /// every byte comes once it reads again (the container waits, as on a
    /// full pipe), and its code.
    fn a_slow_run_misses_nothing() {
        use nix::sys::signal::{Signal, kill};
        let stores = Stores::new();
        let engine = stores.engine();
        let run = stores
            .foreground(
                &["--rm"],
                &["sh", "-c", "head -c 3000000 /dev/zero; exit 4"],
            )
            .spawn()
            .unwrap();
        let pid = nix::unistd::Pid::from_raw(run.id().try_into().unwrap());
        running(&engine);
        kill(pid, Signal::SIGSTOP).unwrap();
        std::thread::sleep(Duration::from_secs(2));
        kill(pid, Signal::SIGCONT).unwrap();
        let ran = run.wait_with_output().unwrap();
        assert_eq!(ran.status.code(), Some(4), "{:?}", ran.status);
        assert_eq!(ran.stdout.len(), 3_000_000);
        assert!(!String::from_utf8_lossy(&ran.stderr).contains("FellBehind"));
    }

    /// A run whose output can't be written any more (`run … | head`) ends
    /// the container as a pipeline would its writer: SIGPIPE (141). One that
    /// ignores SIGPIPE runs on, and the run still passes on its signals as
    /// it waits.
    fn a_closed_output_ends_it_as_a_pipe_would() {
        use nix::sys::signal::{Signal, kill};
        let stores = Stores::new();
        let engine = stores.engine();
        let mut first = [0_u8; 4];
        let mut run = stores
            .foreground(&["--rm"], &["sh", "-c", "while :; do echo yes; done"])
            .spawn()
            .unwrap();
        std::io::Read::read_exact(run.stdout.as_mut().unwrap(), &mut first).unwrap();
        drop(run.stdout.take());
        // Ends of itself, at once: never left writing forever.
        let deadline = Instant::now() + Duration::from_secs(20);
        while run.try_wait().unwrap().is_none() {
            if Instant::now() > deadline {
                let _ = run.kill();
                panic!("the run never ended: its writer wasn't told");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let ran = run.wait_with_output().unwrap();
        assert_eq!(ran.status.code(), Some(141), "{ran:?}");
        let mut run = stores
            .foreground(
                &["--rm"],
                &[
                    "sh",
                    "-c",
                    "trap '' PIPE; while :; do echo yes 2>/dev/null; done",
                ],
            )
            .spawn()
            .unwrap();
        std::io::Read::read_exact(run.stdout.as_mut().unwrap(), &mut first).unwrap();
        drop(run.stdout.take());
        running(&engine);
        std::thread::sleep(Duration::from_millis(500));
        kill(
            nix::unistd::Pid::from_raw(run.id().try_into().unwrap()),
            Signal::SIGTERM,
        )
        .unwrap();
        let ran = run.wait_with_output().unwrap();
        assert_eq!(ran.status.code(), Some(143), "{ran:?}");
    }

    /// A stopped container whose root is a directory of its own
    /// (`--rootfs`): bound as it is, no overlay.
    fn cp_reaches_a_stopped_rootfs_container() {
        let stores = Stores::new();
        let engine = stores.engine();
        let rootfs = shipyard_test_support::rootfs::RootFs::minimal();
        let mut request = stores.request(&["cat", "/in"]);
        request.image = None;
        request.rootfs = Some(rootfs.path().to_owned());
        let record = shipyard_engine::run::create(&engine, request, false).unwrap();
        let name = record.name.as_str();
        let host = stores.scratch.0.join("in");
        std::fs::write(&host, "into a directory\n").unwrap();
        cp(&engine, host.to_str().unwrap(), &format!("{name}:/in")).unwrap();
        assert_eq!(
            std::fs::read_to_string(rootfs.path().join("in")).unwrap(),
            "into a directory\n"
        );
        let out = stores.scratch.0.join("marker");
        cp(&engine, &format!("{name}:/marker"), out.to_str().unwrap()).unwrap();
        assert_eq!(
            std::fs::read_to_string(&out).unwrap(),
            shipyard_test_support::rootfs::MARKER
        );
        shipyard_engine::run::remove(&engine, name, true).unwrap();
    }

    /// `cp -a` into a container that isn't running keeps the archive's
    /// owners, mapped as the container maps them (rootless: its subordinate
    /// IDs; root with `--uidmap`: its own maps), so its start sees them as
    /// given.
    fn a_stopped_cp_keeps_its_owners() {
        let stores = Stores::new();
        let engine = stores.engine();
        let mut archive = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(3);
        header.set_mode(0o644);
        header.set_uid(1000);
        header.set_gid(1001);
        header.set_cksum();
        archive
            .append_data(&mut header, "owned", &b"hi\n"[..])
            .unwrap();
        let archive = archive.into_inner().unwrap();
        let mut requests = vec![stores.request(&["stat", "-c", "%u:%g", "/owned"])];
        // Real root with maps of its own (`--uidmap`): the overlay made by
        // real root, attached in the helper's user namespace (as an init's).
        if shipyard_test_support::is_root() {
            let mut mapped = stores.request(&["stat", "-c", "%u:%g", "/owned"]);
            mapped.uidmap = vec!["0:100000:65536".parse().unwrap()];
            mapped.gidmap = vec!["0:100000:65536".parse().unwrap()];
            requests.push(mapped);
        }
        for request in requests {
            let mapped = !request.uidmap.is_empty();
            let record = shipyard_engine::run::create(&engine, request, false).unwrap();
            let name = record.name.as_str();
            cp_with(&engine, "-", &format!("{name}:/"), &mut &archive[..], true).unwrap();
            if mapped {
                // On the host: shifted by the map.
                let owned = std::fs::metadata(stores.upper(&record.id).join("owned")).unwrap();
                assert_eq!(
                    (
                        std::os::unix::fs::MetadataExt::uid(&owned),
                        std::os::unix::fs::MetadataExt::gid(&owned)
                    ),
                    (101_000, 101_001)
                );
            }
            shipyard_engine::run::start(&engine, name).unwrap();
            assert_eq!(shipyard_engine::run::wait(&engine, name).unwrap(), 0);
            assert_eq!(
                logged(
                    &engine,
                    name,
                    &shipyard_engine::logs::LogsOptions::default()
                ),
                ["1000:1001"],
                "mapped: {mapped}"
            );
            shipyard_engine::run::remove(&engine, name, true).unwrap();
        }
    }

    /// `exec` and `attach` need a container that's running; `logs` and
    /// `cp` don't.
    fn a_stopped_container_cant_be_reached() {
        use shipyard_engine::ErrorKind;
        let stores = Stores::new();
        let engine = stores.engine();
        let record = shipyard_engine::run::create(
            &engine,
            stores.request(&["sh", "-c", "echo said"]),
            false,
        )
        .unwrap();
        let id = record.id.as_str();
        shipyard_engine::run::start(&engine, id).unwrap();
        shipyard_engine::run::wait(&engine, id).unwrap();
        let exec = shipyard_engine::exec::in_container(
            &engine,
            id,
            &shipyard_engine::exec::ExecRequest {
                args: vec!["true".into()],
                ..Default::default()
            },
        );
        assert_eq!(exec.unwrap_err().kind(), ErrorKind::NotRunning);
        let attach = shipyard_engine::attach::to_container(&engine, id, &quiet());
        assert_eq!(attach.unwrap_err().kind(), ErrorKind::NotRunning);
        cp(&engine, &format!("{id}:/marker"), "-").unwrap();
        assert_eq!(
            logged(&engine, id, &shipyard_engine::logs::LogsOptions::default()),
            ["said"]
        );
        shipyard_engine::run::remove(&engine, id, true).unwrap();
    }

    /// F41: while the monitor waits for its state directory's lock at the
    /// end (held here, longer than a client waits to be let in), it still
    /// lets a client in, and tells it the end once the lock frees.
    fn the_monitor_serves_while_it_waits_at_the_end() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id = shipyard_engine::run::create(&engine, stores.request(&["sleep", "300"]), false)
            .unwrap()
            .id;
        shipyard_engine::run::start(&engine, id.as_str()).unwrap();
        let root = stores.runtime_root();
        let init = runtime::state(&root, id.as_str()).unwrap().pid.unwrap();
        let held = nix::fcntl::Flock::lock(
            std::fs::File::open(root.join(id.as_str())).unwrap(),
            nix::fcntl::FlockArg::LockExclusive,
        )
        .unwrap();
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(init),
            nix::sys::signal::Signal::SIGKILL,
        )
        .unwrap();
        eventually("its init gone, its monitor waiting", || {
            runtime::peek_status(&root, id.as_str()).unwrap() == Some(runtime::Status::Stopping)
        });
        let waiter = {
            let root = root.clone();
            let id = id.clone();
            std::thread::spawn(move || runtime::wait(&root, id.as_str()))
        };
        // Past the 5 s a client waits to be let in.
        std::thread::sleep(Duration::from_secs(6));
        drop(held);
        assert_eq!(waiter.join().unwrap().unwrap(), Some(137));
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// A `--rm` container's own clean-up (which takes no operation lock)
    /// racing a `start` and an `rm` of it: each ends cleanly (done, or "no
    /// such container"), and nothing of it is left.
    fn start_and_rm_beside_an_rm_containers_end() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id =
            shipyard_engine::run::create(&engine, stores.request(&["sh", "-c", "exit 0"]), true)
                .unwrap()
                .id;
        shipyard_engine::run::start(&engine, id.as_str()).unwrap();
        for command in ["start", "rm", "start", "rm"] {
            let output = stores
                .container_command(command, id.as_str())
                .output()
                .unwrap();
            let said = String::from_utf8_lossy(&output.stdout);
            assert!(
                said.contains("ok") || said.contains("error=NotFound"),
                "{command}: {said}"
            );
        }
        eventually("the container gone", || {
            containers::testing::find(&engine, id.as_str()).is_err()
        });
        eventually("its area gone", || stores.areas().is_empty());
    }

    /// A container's life in the journal, in order: made (its volume too,
    /// on first use), started, ended (with its code, at its exit's time),
    /// renamed, removed; the CLI's events and its clean-up's in one journal.
    fn events_tell_a_containers_life() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id = Stores::id(&stores.launch(&[
            "--network",
            "none",
            "--volume",
            "evdata:/data",
            stores.image.as_str(),
            "sh",
            "-c",
            "exit 3",
        ]));
        eventually("its end", || stores.record(&id).exit.is_some());
        eventually("its die", || {
            stores.actions(id.as_str()).contains(&"die".to_owned())
        });
        let old = stores.record(&id).name.as_str().to_owned();
        let finished = stores.record(&id).finished.unwrap();
        let started = stores.record(&id).started.unwrap();
        shipyard_engine::run::rename(&engine, id.as_str(), "renamed").unwrap();
        shipyard_engine::run::remove(&engine, id.as_str(), false).unwrap();
        shipyard_engine::volumes::remove(&engine, "evdata").unwrap();
        assert_eq!(
            stores.actions(id.as_str()),
            ["create", "start", "die", "rename", "destroy"]
        );
        // Its image removed: untagged, then deleted (the image, by its
        // manifest's digest: not each of its blobs).
        shipyard_engine::image::remove(&engine, &stores.image, false).unwrap();
        let images: Vec<(String, String)> = stores
            .events()
            .into_iter()
            .filter(|event| event.kind == shipyard_engine::events::Kind::Image)
            .map(|event| (event.action, event.id))
            .collect();
        let actions: Vec<&str> = images.iter().map(|(action, _)| action.as_str()).collect();
        assert_eq!(actions, ["pull", "untag", "delete"], "{images:?}");
        assert!(images[2].1.starts_with("sha256:"), "{images:?}");
        assert_eq!(stores.actions("evdata"), ["create", "destroy"]);
        let events = stores.events();
        let about = |action: &str| {
            events
                .iter()
                .find(|event| event.id == id.as_str() && event.action == action)
                .unwrap()
                .clone()
        };
        let die = about("die");
        assert_eq!(die.attributes["exitCode"], "3");
        assert_eq!(die.attributes["name"], old);
        assert_eq!(die.attributes["image"], stores.image);
        assert_eq!(die.time, finished, "its exit's time");
        assert_eq!(
            about("start").time,
            started,
            "its start's time, as recorded"
        );
        assert!(die.time <= about("rename").time);
        assert!(about("start").time <= die.time);
        assert_eq!(about("rename").attributes["oldName"], old);
        assert_eq!(about("destroy").attributes["name"], "renamed");
    }

    /// A restart policy's starts say which restart each is.
    fn a_policy_restarts_events_count_it() {
        let stores = Stores::new();
        let id = stores.launch_restarting("on-failure:2", "exit 3");
        eventually("two restarts, then the end", || {
            let record = stores.record(&id);
            record.restart_count == 2 && record.next_start.is_none() && record.exit.is_some()
        });
        eventually("three dies", || {
            stores
                .actions(id.as_str())
                .iter()
                .filter(|action| *action == "die")
                .count()
                == 3
        });
        let counts: Vec<Option<String>> = stores
            .events()
            .into_iter()
            .filter(|event| event.id == id.as_str() && event.action == "start")
            .map(|event| event.attributes.get("restartCount").cloned())
            .collect();
        assert_eq!(counts, [None, Some("1".to_owned()), Some("2".to_owned())]);
        shipyard_engine::run::remove(&stores.engine(), id.as_str(), false).unwrap();
    }

    /// A container whose monitor was killed: the recovery that settles it
    /// writes its die, dated when it was found to have ended, without a
    /// code (how isn't known).
    fn a_settled_die_is_dated_when_it_ended() {
        let stores = Stores::new();
        let id = Stores::id(&stores.launch(&[
            "--network",
            "none",
            stores.image.as_str(),
            "sleep",
            "300",
        ]));
        stores.kill_monitor(&id);
        let engine = stores.engine();
        // A create runs the recovery pass: it settles this one.
        let other =
            shipyard_engine::run::create(&engine, stores.request(&["true"]), false).unwrap();
        let finished = stores.record(&id).finished.unwrap();
        let dies: Vec<_> = stores
            .events()
            .into_iter()
            .filter(|event| event.id == id.as_str() && event.action == "die")
            .collect();
        assert_eq!(dies.len(), 1, "{dies:?}");
        assert_eq!(dies[0].time, finished);
        assert!(!dies[0].attributes.contains_key("exitCode"));
        shipyard_engine::run::remove(&engine, other.id.as_str(), false).unwrap();
        shipyard_engine::run::remove(&engine, id.as_str(), false).unwrap();
    }

    /// The OOM killer's kill: `oom`, then `die` (137). Root: a memory limit
    /// needs a cgroup of its own.
    fn an_oom_kill_says_oom_then_die() {
        if !is_root() {
            return;
        }
        let stores = Stores::new();
        let id = Stores::id(&stores.launch(&[
            "--network",
            "none",
            "--small-memory",
            stores.image.as_str(),
            "sh",
            "-c",
            "x=$(head -c 200m /dev/urandom); echo survived",
        ]));
        eventually("its die", || {
            stores.actions(id.as_str()).contains(&"die".to_owned())
        });
        let events: Vec<_> = stores
            .events()
            .into_iter()
            .filter(|event| event.id == id.as_str())
            .collect();
        let actions: Vec<&str> = events.iter().map(|event| event.action.as_str()).collect();
        assert_eq!(actions, ["create", "start", "oom", "die"]);
        assert_eq!(events[3].attributes["exitCode"], "137");
        shipyard_engine::run::remove(&stores.engine(), id.as_str(), false).unwrap();
    }

    /// A container whose clean-up was killed before it recorded the end
    /// (its monitor's exit record kept): the next command that takes it
    /// settles it, and says its end, once. (The clean-up waits for its
    /// monitor to go first: it's killed then; tried again if it was too
    /// quick.)
    fn an_end_its_clean_up_missed_is_said_once() {
        let stores = Stores::new();
        let engine = stores.engine();
        for _ in 0..5 {
            let id = Stores::id(&stores.launch(&[
                "--network",
                "none",
                stores.image.as_str(),
                "sh",
                "-c",
                "sleep 1; exit 4",
            ]));
            let deadline = Instant::now() + Duration::from_secs(20);
            let killed = loop {
                let clean_up = processes_with("__cleanup")
                    .into_iter()
                    .find(|line| line.contains(id.as_str()));
                if let Some(line) = clean_up {
                    let pid: i32 = line.split(':').next().unwrap().parse().unwrap();
                    let _ = nix::sys::signal::kill(
                        nix::unistd::Pid::from_raw(pid),
                        nix::sys::signal::Signal::SIGKILL,
                    );
                    break true;
                }
                if stores.record(&id).exit.is_some() {
                    break false;
                }
                assert!(Instant::now() < deadline, "no clean-up seen");
            };
            if !killed || stores.record(&id).exit.is_some() {
                // Too late: it had recorded the end. Again.
                eventually("its clean-up done", || stores.record(&id).exit.is_some());
                shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
                continue;
            }
            shipyard_engine::run::remove(&engine, id.as_str(), false).unwrap();
            assert_eq!(
                stores.actions(id.as_str()),
                ["create", "start", "die", "destroy"]
            );
            let die = stores
                .events()
                .into_iter()
                .find(|event| event.id == id.as_str() && event.action == "die")
                .unwrap();
            assert_eq!(die.attributes["exitCode"], "4");
            return;
        }
        panic!("the clean-up recorded the end first, every time");
    }

    /// `rm -f` of a running container, as Docker says it: `kill`, its end
    /// (`die`, 137: killed by its monitor, its exit recorded), `destroy`.
    fn rm_f_says_kill_die_destroy() {
        let stores = Stores::new();
        let id = Stores::id(&stores.launch(&[
            "--network",
            "none",
            stores.image.as_str(),
            "sleep",
            "300",
        ]));
        shipyard_engine::run::remove(&stores.engine(), id.as_str(), true).unwrap();
        assert_eq!(
            stores.actions(id.as_str()),
            ["create", "start", "kill", "die", "destroy"]
        );
        let die = stores
            .events()
            .into_iter()
            .find(|event| event.id == id.as_str() && event.action == "die")
            .unwrap();
        assert_eq!(die.attributes["exitCode"], "137");
    }

    /// `rm` of a container whose monitor was killed: its end, nothing
    /// recorded it, is settled and said (no code) before it goes.
    fn rm_of_an_end_unwatched_says_its_die() {
        let stores = Stores::new();
        let id = Stores::id(&stores.launch(&[
            "--network",
            "none",
            stores.image.as_str(),
            "sleep",
            "300",
        ]));
        stores.kill_monitor(&id);
        shipyard_engine::run::remove(&stores.engine(), id.as_str(), false).unwrap();
        assert_eq!(
            stores.actions(id.as_str()),
            ["create", "start", "die", "destroy"]
        );
    }

    /// A container with the health check `script` (see `launch`'s
    /// `--health`), running `sleep 300`, its other flags `extra`.
    fn launch_checked(stores: &Stores, script: &str, extra: &[&str]) -> ContainerId {
        let mut args = vec!["--network", "none", "--health", script];
        args.extend_from_slice(extra);
        args.extend([stores.image.as_str(), "sleep", "300"]);
        Stores::id(&stores.launch(&args))
    }

    /// What container `id`'s checks say now.
    fn health_of(stores: &Stores, id: &ContainerId) -> Option<shipyard_engine::health::Health> {
        let engine = stores.engine();
        let record = stores.record(id);
        let state = containers::state(&engine, &record).unwrap();
        containers::health(&engine, &record, &state)
    }

    /// The `healthStatus`es of container `id`'s `health_status` events.
    fn health_events(stores: &Stores, id: &ContainerId) -> Vec<String> {
        stores
            .events()
            .into_iter()
            .filter(|event| event.id == id.as_str() && event.action == "health_status")
            .map(|event| event.attributes["healthStatus"].clone())
            .collect()
    }

    /// The `__health` companions running for container `id`.
    fn companions(id: &ContainerId) -> Vec<String> {
        processes_with("__health ")
            .into_iter()
            .filter(|line| line.contains(id.as_str()))
            .collect()
    }

    /// A check that passes: healthy, said once; its result kept.
    fn a_passing_check_is_healthy() {
        use shipyard_engine::health::Status;
        let stores = Stores::new();
        let id = launch_checked(&stores, "true", &[]);
        eventually("healthy", || {
            health_of(&stores, &id).is_some_and(|health| health.status == Status::Healthy)
        });
        let health = health_of(&stores, &id).unwrap();
        assert_eq!(health.failing_streak, 0);
        assert_eq!(health.log.last().unwrap().exit_code, 0);
        assert_eq!(health_events(&stores, &id), ["healthy"]);
        assert_eq!(companions(&id).len(), 1, "{:?}", companions(&id));
        // Budgets (H7): a check (a process, then `exec`) well under a second;
        // the companion small, as the monitor is (its own memory, not its
        // binary's text).
        let took = |probe: &shipyard_engine::health::Probe| {
            let at = |text: &str| {
                let time = shipyard_runtime::logs::time::Timestamp::parse(text).unwrap();
                shipyard_engine::events::system_time(time).unwrap()
            };
            at(&probe.end).duration_since(at(&probe.start)).unwrap()
        };
        let fastest = health.log.iter().map(took).min().unwrap();
        assert!(fastest < Duration::from_secs(1), "a check took {fastest:?}");
        let pid = companions(&id)[0].split(':').next().unwrap().to_owned();
        let rollup = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).unwrap();
        let anonymous: u64 = rollup
            .lines()
            .find_map(|line| line.strip_prefix("Anonymous:"))
            .and_then(|kb| kb.trim().trim_end_matches("kB").trim().parse().ok())
            .unwrap();
        assert!(anonymous < 2048, "the companion: {anonymous} kB");
        shipyard_engine::run::remove(&stores.engine(), id.as_str(), true).unwrap();
        eventually("its companion gone", || companions(&id).is_empty());
    }

    /// A check reads its own container's record, by its ID, and no other
    /// (stage 9, step 9.9a, L1): another record in the store that can't be
    /// read leaves its checks and its log alone. (Every record read for each
    /// check would cost each container the whole store, every interval, and
    /// say each unreadable one in every container's log.)
    fn a_check_reads_its_record_alone() {
        use shipyard_engine::health::Status;
        let stores = Stores::new();
        let id = launch_checked(&stores, "true", &[]);
        eventually("healthy", || {
            health_of(&stores, &id).is_some_and(|health| health.status == Status::Healthy)
        });
        let containers = stores.scratch.0.join("containers");
        let broken = containers.join("f".repeat(64));
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("record.json"), "not a record").unwrap();
        // The control: a listing says it.
        let (engine, seen) = stores.engine_collecting();
        containers::list(&engine).unwrap();
        assert!(
            seen.events().iter().any(|event| matches!(
                event,
                Message::Warning(warning) if warning.kind == WarningKind::RecordUnreadable
            )),
            "{:?}",
            seen.events()
        );
        // Checks go on (every 300 ms), and say nothing of it.
        let checked = |health: Option<shipyard_engine::health::Health>| {
            health.map(|health| health.log.last().map(|probe| probe.start.clone()))
        };
        let before = checked(health_of(&stores, &id));
        std::thread::sleep(Duration::from_secs(2));
        let health = health_of(&stores, &id).unwrap();
        assert_eq!(health.status, Status::Healthy);
        assert_ne!(checked(Some(health)), before, "no check since");
        let log = std::fs::read_to_string(containers.join(id.as_str()).join("health.log"))
            .unwrap_or_default();
        assert!(!log.contains("left out"), "{log}");
        std::fs::remove_dir_all(&broken).unwrap();
        shipyard_engine::run::remove(&stores.engine(), id.as_str(), true).unwrap();
    }

    /// A check that fails: starting through the first two failures, then
    /// unhealthy at the third (its output kept); a check that can't run
    /// (127) is a failure too.
    fn failing_checks_make_it_unhealthy() {
        use shipyard_engine::health::Status;
        let stores = Stores::new();
        let id = launch_checked(&stores, "echo sick; exit 1", &[]);
        eventually("unhealthy", || {
            health_of(&stores, &id).is_some_and(|health| health.status == Status::Unhealthy)
        });
        let health = health_of(&stores, &id).unwrap();
        assert!(health.failing_streak >= 3, "{health:?}");
        assert_eq!(health.log[0].output, "sick");
        assert_eq!(health.log[0].exit_code, 1);
        assert_eq!(health_events(&stores, &id), ["unhealthy"]);
        shipyard_engine::run::remove(&stores.engine(), id.as_str(), true).unwrap();
        let missing = launch_checked(&stores, "exec /no/such/check", &["--health-retries", "1"]);
        eventually("unhealthy", || {
            health_of(&stores, &missing).is_some_and(|health| health.status == Status::Unhealthy)
        });
        assert_eq!(health_of(&stores, &missing).unwrap().log[0].exit_code, 127);
        shipyard_engine::run::remove(&stores.engine(), missing.as_str(), true).unwrap();
    }

    /// A check that doesn't answer in time is a failure, said as Docker
    /// says it; it's ended, in the container too.
    fn a_check_out_of_time_fails_and_leaves_nothing() {
        use shipyard_engine::health::Status;
        let stores = Stores::new();
        let id = launch_checked(
            &stores,
            "sleep 77",
            &["--health-timeout-ms", "500", "--health-retries", "1"],
        );
        eventually("unhealthy", || {
            health_of(&stores, &id).is_some_and(|health| health.status == Status::Unhealthy)
        });
        let health = health_of(&stores, &id).unwrap();
        assert_eq!(health.log[0].exit_code, -1);
        assert_eq!(
            health.log[0].output,
            "Health check exceeded timeout (500ms)"
        );
        // Each check's process group killed at its timeout (the shell's
        // child too, which has no parent-death signal): never more than the
        // one check under way.
        let sleeping = || processes_with("sleep 77");
        std::thread::sleep(Duration::from_secs(2));
        assert!(sleeping().len() <= 2, "{:?}", sleeping());
        shipyard_engine::run::remove(&stores.engine(), id.as_str(), true).unwrap();
        eventually("no check left", || sleeping().is_empty());
    }

    /// Failures in the start period don't count; after it, they do.
    fn the_start_period_forgives_failures() {
        use shipyard_engine::health::Status;
        let stores = Stores::new();
        let id = launch_checked(
            &stores,
            "exit 1",
            &["--health-start-period-ms", "2500", "--health-retries", "1"],
        );
        eventually("two failures", || {
            health_of(&stores, &id).is_some_and(|health| health.log.len() >= 2)
        });
        let early = health_of(&stores, &id).unwrap();
        assert_eq!(
            (early.status, early.failing_streak),
            (Status::Starting, 0),
            "{early:?}"
        );
        eventually("unhealthy once the period is over", || {
            health_of(&stores, &id).is_some_and(|health| health.status == Status::Unhealthy)
        });
        shipyard_engine::run::remove(&stores.engine(), id.as_str(), true).unwrap();
    }

    /// Each start has its companion, and its own health (starting
    /// afresh); a stop ends it. A container without a check has none.
    fn the_companion_goes_with_each_start() {
        use shipyard_engine::health::Status;
        let stores = Stores::new();
        let engine = stores.engine();
        let id = launch_checked(&stores, "true", &[]);
        eventually("healthy", || {
            health_of(&stores, &id).is_some_and(|health| health.status == Status::Healthy)
        });
        let fast = shipyard_engine::run::StopOptions {
            timeout: Some(Duration::ZERO),
            ..Default::default()
        };
        shipyard_engine::run::stop(&engine, id.as_str(), fast).unwrap();
        assert!(companions(&id).is_empty(), "{:?}", companions(&id));
        assert_eq!(health_of(&stores, &id), None, "none while it's down");
        shipyard_engine::run::start(&engine, id.as_str()).unwrap();
        eventually("healthy again", || {
            health_of(&stores, &id)
                .is_some_and(|health| health.status == Status::Healthy && health.log.len() < 5)
        });
        assert_eq!(companions(&id).len(), 1);
        assert_eq!(health_events(&stores, &id), ["healthy", "healthy"]);
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
        let plain = Stores::id(&stores.launch(&[
            "--network",
            "none",
            stores.image.as_str(),
            "sleep",
            "300",
        ]));
        std::thread::sleep(Duration::from_millis(500));
        assert!(companions(&plain).is_empty());
        assert_eq!(health_of(&stores, &plain), None);
        shipyard_engine::run::remove(&engine, plain.as_str(), true).unwrap();
    }

    /// A paused container isn't checked (a check under way when it's
    /// paused counts for nothing); its checks go on once it's resumed.
    /// Root: a pause needs a cgroup of the container's own.
    fn a_paused_container_isnt_checked() {
        use shipyard_engine::health::Status;
        if !is_root() {
            return;
        }
        let stores = Stores::new();
        // A check that takes most of each round: the pause comes during one.
        let id = launch_checked(&stores, "sleep 1", &[]);
        eventually("healthy", || {
            health_of(&stores, &id).is_some_and(|health| health.status == Status::Healthy)
        });
        // Into the next check (after its 300 ms wait), not between two.
        std::thread::sleep(Duration::from_millis(600));
        runtime::pause(&stores.runtime_root(), id.as_str()).unwrap();
        std::thread::sleep(Duration::from_millis(500));
        let paused = health_of(&stores, &id).unwrap();
        // Past the check's timeout (2 s): the check under way, frozen,
        // ends while it's paused.
        std::thread::sleep(Duration::from_millis(3000));
        assert_eq!(
            health_of(&stores, &id).unwrap(),
            paused,
            "checked while paused"
        );
        assert_eq!(companions(&id).len(), 1, "its companion went");
        runtime::resume(&stores.runtime_root(), id.as_str()).unwrap();
        eventually("checked again", || {
            health_of(&stores, &id).is_some_and(|health| health.log != paused.log)
        });
        assert_eq!(health_events(&stores, &id), ["healthy"]);
        shipyard_engine::run::remove(&stores.engine(), id.as_str(), true).unwrap();
    }

    /// A check under way when the container ends (a stop: its command
    /// killed with it, a failure) counts for nothing: nothing is written
    /// or said of it.
    fn a_check_under_way_at_the_end_counts_for_nothing() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id = launch_checked(
            &stores,
            "sleep 3; exit 1",
            &["--health-retries", "1", "--health-timeout-ms", "10000"],
        );
        // Its first check under way (begun after 300 ms).
        std::thread::sleep(Duration::from_millis(1500));
        let fast = shipyard_engine::run::StopOptions {
            timeout: Some(Duration::ZERO),
            ..Default::default()
        };
        shipyard_engine::run::stop(&engine, id.as_str(), fast).unwrap();
        std::thread::sleep(Duration::from_secs(3));
        let logs = containers::testing::logs_dir(&engine, &id).unwrap();
        let start = std::fs::read_dir(&logs)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .max()
            .unwrap();
        let health = shipyard_engine::health::Health::read(&start).unwrap();
        assert_eq!(health.status, shipyard_engine::health::Status::Starting);
        assert!(health.log.is_empty(), "{health:?}");
        assert!(health_events(&stores, &id).is_empty());
        shipyard_engine::run::remove(&engine, id.as_str(), false).unwrap();
    }

    /// A check that leaves a process behind in the container, out of its
    /// process group (a session of its own, made before `exec` ends the
    /// group), which keeps the check's output open, is done once it exits:
    /// its answer counts (not a timeout), with what it said.
    fn a_check_that_leaves_a_process_still_answers() {
        use shipyard_engine::health::Status;
        let stores = Stores::new();
        let id = launch_checked(&stores, "setsid sleep 60 & sleep 0.2; echo up", &[]);
        eventually("healthy", || {
            health_of(&stores, &id).is_some_and(|health| health.status == Status::Healthy)
        });
        let health = health_of(&stores, &id).unwrap();
        assert_eq!(health.log[0].exit_code, 0);
        assert_eq!(health.log[0].output, "up");
        shipyard_engine::run::remove(&stores.engine(), id.as_str(), true).unwrap();
    }

    /// An image's own health check (its config's `Healthcheck`, quick:
    /// every 300 ms), of `test` (JSON).
    fn checked_image(test: &str) -> Stores {
        Stores::with_image_config(&format!(
            r#"{{"Cmd":["/bin/cat","/marker"],"Healthcheck":{{"Test":{test},
            "Interval":300000000,"StartInterval":300000000,"Timeout":2000000000}}}}"#
        ))
    }

    /// A container of `stores`' image, running `sleep 300`, its flags
    /// `extra`.
    fn launch_plain(stores: &Stores, extra: &[&str]) -> ContainerId {
        let mut args = vec!["--network", "none"];
        args.extend_from_slice(extra);
        args.extend([stores.image.as_str(), "sleep", "300"]);
        Stores::id(&stores.launch(&args))
    }

    /// An image's own check runs with no flag, settled in the container's
    /// config at create (as Docker's `inspect` shows it, durations in
    /// milliseconds); `--no-healthcheck` turns it off.
    fn the_images_check_is_used() {
        use shipyard_engine::health::Status;
        let stores = checked_image(r#"["CMD-SHELL","true"]"#);
        let id = launch_plain(&stores, &[]);
        eventually("healthy", || {
            health_of(&stores, &id).is_some_and(|health| health.status == Status::Healthy)
        });
        let check = stores.record(&id).config.health.unwrap();
        assert_eq!(check.test, ["CMD-SHELL", "true"]);
        assert_eq!((check.interval_ms, check.timeout_ms), (300, 2000));
        shipyard_engine::run::remove(&stores.engine(), id.as_str(), true).unwrap();
        let off = launch_plain(&stores, &["--no-healthcheck"]);
        std::thread::sleep(Duration::from_millis(800));
        assert_eq!(stores.record(&off).config.health, None);
        assert!(companions(&off).is_empty());
        assert_eq!(health_of(&stores, &off), None);
        shipyard_engine::run::remove(&stores.engine(), off.as_str(), true).unwrap();
    }

    /// Flags without a command tune the image's check (its test kept,
    /// their retries used); `--health-cmd` replaces its test (its settings
    /// kept for what the flags leave out).
    fn flags_tune_or_replace_the_images_check() {
        use shipyard_engine::health::Status;
        let stores = checked_image(r#"["CMD-SHELL","exit 1"]"#);
        let tuned = launch_plain(&stores, &["--tune-retries", "1"]);
        eventually("unhealthy", || {
            health_of(&stores, &tuned).is_some_and(|health| health.status == Status::Unhealthy)
        });
        let check = stores.record(&tuned).config.health.unwrap();
        assert_eq!((check.test[1].as_str(), check.retries), ("exit 1", 1));
        assert_eq!(check.interval_ms, 300, "the image's, the flag's zero left");
        shipyard_engine::run::remove(&stores.engine(), tuned.as_str(), true).unwrap();
        let replaced = launch_plain(&stores, &["--health", "true"]);
        eventually("healthy", || {
            health_of(&stores, &replaced).is_some_and(|health| health.status == Status::Healthy)
        });
        let replaced_check = stores.record(&replaced).config.health.unwrap();
        assert_eq!(replaced_check.test, ["CMD-SHELL", "true"]);
        assert_eq!(replaced_check.interval_ms, 300, "the image's interval kept");
        shipyard_engine::run::remove(&stores.engine(), replaced.as_str(), true).unwrap();
    }

    /// Flags that tune a check there isn't (no `--health-cmd`, an image
    /// without one): said, and no check.
    fn tuning_a_check_there_isnt_is_said() {
        let stores = Stores::new();
        let (engine, seen) = stores.engine_collecting();
        let mut request = stores.request(&["sleep", "300"]);
        request.health = Some(shipyard_engine::health::HealthCheck {
            test: Vec::new(),
            retries: 1,
            ..shipyard_engine::health::HealthCheck::shell("")
        });
        let made = shipyard_engine::run::create(&engine, request, false).unwrap();
        assert_eq!(made.config.health, None);
        assert!(
            seen.warnings()
                .iter()
                .any(|warning| warning.kind == WarningKind::HealthCheckIgnored),
            "{:?}",
            seen.warnings()
        );
        shipyard_engine::run::remove(&engine, made.id.as_str(), false).unwrap();
    }

    /// An image's check that's `NONE` is none (unless `--health-cmd` gives
    /// one); one that can't be run is left out, said, the container made all
    /// the same; a shell-form check runs with the image's `SHELL`.
    fn an_images_none_or_bad_check_and_its_shell() {
        use shipyard_engine::health::Status;
        let none = checked_image(r#"["NONE"]"#);
        let id = launch_plain(&none, &[]);
        assert_eq!(none.record(&id).config.health, None);
        shipyard_engine::run::remove(&none.engine(), id.as_str(), true).unwrap();
        let given = launch_plain(&none, &["--health", "true"]);
        assert!(none.record(&given).config.health.is_some());
        shipyard_engine::run::remove(&none.engine(), given.as_str(), true).unwrap();
        let bad = checked_image(r#"["CMD"]"#);
        let (engine, seen) = bad.engine_collecting();
        let made =
            shipyard_engine::run::create(&engine, bad.request(&["sleep", "300"]), false).unwrap();
        assert_eq!(made.config.health, None);
        assert!(
            seen.warnings()
                .iter()
                .any(|warning| warning.kind == WarningKind::HealthCheckIgnored),
            "{:?}",
            seen.warnings()
        );
        shipyard_engine::run::remove(&engine, made.id.as_str(), false).unwrap();
        // With `cat` as its shell, `/marker` is read (passes); with
        // `/bin/sh -c`, it would be run (and fail).
        let shelled = Stores::with_image_config(
            r#"{"Cmd":["/bin/cat","/marker"],"Shell":["/bin/cat"],"Healthcheck":{
            "Test":["CMD-SHELL","/marker"],"Interval":300000000,"StartInterval":300000000}}"#,
        );
        let id = launch_plain(&shelled, &[]);
        assert_eq!(
            shelled.record(&id).config.health.unwrap().shell,
            ["/bin/cat"]
        );
        eventually("healthy", || {
            health_of(&shelled, &id).is_some_and(|health| health.status == Status::Healthy)
        });
        shipyard_engine::run::remove(&shelled.engine(), id.as_str(), true).unwrap();
    }

    /// A container's pasta killed while it runs: the container runs on,
    /// without a network, and that's said (the start's marker, `ps`'s
    /// state, a `network_lost` event).
    fn a_dead_pasta_is_said() {
        if !pasta_here() {
            return;
        }
        let stores = Stores::new();
        let _upstream = is_root().then(|| {
            let upstream = shipyard_test_support::net::Upstream::new();
            upstream.route_default();
            upstream
        });
        let before = stores.run_dirs();
        let id = Stores::id(&stores.launch(&[
            "--network",
            "pasta",
            stores.image.as_str(),
            "sleep",
            "300",
        ]));
        let pasta = stores.pasta_pid(&before).expect("pasta.pid");
        let engine = stores.engine();
        assert_eq!(
            containers::last_start_lost(&engine, &stores.record(&id)),
            None
        );
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pasta),
            nix::sys::signal::Signal::SIGKILL,
        )
        .unwrap();
        eventually("its loss said", || {
            containers::last_start_lost(&engine, &stores.record(&id)).is_some()
        });
        let lost = containers::last_start_lost(&engine, &stores.record(&id)).unwrap();
        assert_eq!(lost.how, "pasta was killed (SIGKILL)");
        assert!(matches!(stores.state(&id), ContainerState::Running { .. }));
        let summary = containers::summaries(&engine, false)
            .unwrap()
            .into_iter()
            .find(|summary| summary.record.id == id)
            .unwrap();
        assert_eq!(summary.network_lost, Some(lost));
        eventually("its event", || {
            stores
                .actions(id.as_str())
                .contains(&"network_lost".to_owned())
        });
        // Stopped: no network to have lost (`ps -a` says how it ended).
        let fast = shipyard_engine::run::StopOptions {
            timeout: Some(Duration::ZERO),
            ..Default::default()
        };
        shipyard_engine::run::stop(&engine, id.as_str(), fast).unwrap();
        let stopped = containers::summaries(&engine, true)
            .unwrap()
            .into_iter()
            .find(|summary| summary.record.id == id)
            .unwrap();
        assert_eq!(stopped.network_lost, None);
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// A pasta that ends by itself once ready (a fake: it says it's ready,
    /// then exits 1): said with its code.
    fn a_pasta_that_ends_is_said_with_its_code() {
        let stores = Stores::new();
        let output = stores.launch_with_fake_pasta(
            "while [ $# -gt 0 ]; do case \"$1\" in --pid) echo $$ > \"$2\"; shift;; esac; \
             shift; done; sleep 1; exit 1",
            &["--network", "pasta", stores.image.as_str(), "sleep", "300"],
        );
        let id = Stores::id(&output);
        let engine = stores.engine();
        eventually("its loss said", || {
            containers::last_start_lost(&engine, &stores.record(&id)).is_some()
        });
        assert_eq!(
            containers::last_start_lost(&engine, &stores.record(&id))
                .unwrap()
                .how,
            "pasta ended (code 1)"
        );
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// pasta and the init killed together (the init first): the launcher
    /// sees pasta reaped with the init already gone, which is the
    /// container's end, not a lost network: nothing is said.
    fn a_pasta_ending_with_its_init_isnt_a_loss() {
        if !pasta_here() {
            return;
        }
        let stores = Stores::new();
        let _upstream = is_root().then(|| {
            let upstream = shipyard_test_support::net::Upstream::new();
            upstream.route_default();
            upstream
        });
        let before = stores.run_dirs();
        let id = Stores::id(&stores.launch(&[
            "--network",
            "pasta",
            stores.image.as_str(),
            "sleep",
            "300",
        ]));
        let pasta = stores.pasta_pid(&before).expect("pasta.pid");
        let init = runtime::state(&stores.runtime_root(), id.as_str())
            .unwrap()
            .pid
            .unwrap();
        for pid in [init, pasta] {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        eventually("its end", || stores.record(&id).exit.is_some());
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(
            containers::last_start_lost(&stores.engine(), &stores.record(&id)),
            None
        );
        assert!(
            !stores
                .actions(id.as_str())
                .contains(&"network_lost".to_owned())
        );
        shipyard_engine::run::remove(&stores.engine(), id.as_str(), false).unwrap();
    }

    /// A user's starts keep the latest ten starts' logs too, as a policy's
    /// restarts do (stage 9, step 9.9c, R4): a container started again and
    /// again doesn't fill the disk. (Older starts planted between its
    /// starts stand for many.)
    fn a_users_starts_keep_the_latest_ten_starts_logs() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id = Stores::id(&stores.launch(&["--network", "none", stores.image.as_str(), "true"]));
        let ended = || {
            eventually("its end, recorded", || {
                matches!(stores.state(&id), ContainerState::Exited { .. })
                    && stores.running_file(&id).try_lock().is_ok()
            });
        };
        ended();
        let logs = containers::testing::logs_dir(&engine, &id).unwrap();
        let first = names(&logs);
        for older in 0..12 {
            std::fs::create_dir(logs.join(format!("{}", 1000 + older))).unwrap();
        }
        for _ in 0..3 {
            shipyard_engine::run::start(&engine, id.as_str()).unwrap();
            ended();
        }
        let kept = names(&logs);
        assert_eq!(kept.len(), 10, "{kept:?}");
        assert!(!kept.contains(&first[0]), "the oldest kept: {kept:?}");
        shipyard_engine::run::remove(&engine, id.as_str(), false).unwrap();
    }

    /// An end `__cleanup` couldn't record (the store refused the write: a
    /// full disk's `ENOSPC`; here, a read-only record directory) is retried
    /// until the store takes it (F45): until then the end isn't recorded,
    /// the runtime keeps its exit record, and a refused retry tries again;
    /// then it's recorded, and the `always` policy starts it again.
    /// Rootless: root's writes aren't refused.
    fn an_end_the_store_refused_is_retried() {
        use std::os::unix::fs::PermissionsExt as _;
        if is_root() {
            return;
        }
        let stores = Stores::new();
        let id = stores.launch_restarting("always", "sleep 300");
        eventually("it runs", || {
            matches!(stores.state(&id), ContainerState::Running { .. })
        });
        let dir = stores.scratch.0.join("containers").join(id.as_str());
        // Its log made first: written to, read-only directory or not.
        let log = dir.join("cleanup.log");
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .unwrap();
        let first = stores.record(&id);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        runtime::kill(
            &stores.runtime_root(),
            id.as_str(),
            shipyard_runtime::process::Signal::KILL,
        )
        .unwrap();
        eventually("the refused write, said", || {
            stores.cleanup_log(&id).contains("the record:")
        });
        assert_eq!(stores.record(&id).finished, first.finished, "not recorded");
        assert!(
            runtime::status(&stores.runtime_root(), id.as_str())
                .unwrap()
                .is_some(),
            "the runtime keeps how it ended"
        );
        // A retry refused too, so a retry that gave up at its first
        // refusal would never record it.
        eventually("a retry refused, said", || {
            stores
                .cleanup_log(&id)
                .contains("the end's retry: still refused")
        });
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        eventually("recorded, and started again", || {
            let record = stores.record(&id);
            record.restart_count >= 1 && matches!(stores.state(&id), ContainerState::Running { .. })
        });
        shipyard_engine::run::remove(&stores.engine(), id.as_str(), true).unwrap();
    }

    /// The pass that settles an end nothing recorded forgets what that
    /// start kept aside (stage 10's close, Murphy): left, a later start
    /// cut short (its own keeping aside failed) would restore a start two
    /// back.
    fn an_end_settled_by_a_pass_forgets_the_start_kept_aside() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id = Stores::id(&stores.launch(&[
            "--network",
            "none",
            stores.image.as_str(),
            "sh",
            "-c",
            "exit 3",
        ]));
        eventually("its end, recorded", || {
            matches!(
                stores.state(&id),
                ContainerState::Exited { code: Some(3), .. }
            ) && stores.running_file(&id).try_lock().is_ok()
        });
        // An end nothing recorded (its clean-up killed before it wrote),
        // and an older start's file left beside it.
        let dir = containers::testing::record_dir(&engine, &id).unwrap();
        let older = serde_json::json!({"version": 1, "started": null, "finished": null,
                                       "exit": null, "boot_id": null});
        std::fs::write(dir.join("previous-start.json"), older.to_string()).unwrap();
        containers::testing::update(&engine, &id, |record| {
            record.finished = None;
            record.exit = None;
        })
        .unwrap();
        // The next pass (any container-changing command's) settles it.
        let other =
            shipyard_engine::run::create(&engine, stores.request(&["true"]), false).unwrap();
        shipyard_engine::run::remove(&engine, other.id.as_str(), false).unwrap();
        assert!(stores.record(&id).finished.is_some(), "its end settled");
        assert!(
            !dir.join("previous-start.json").exists(),
            "an older start's file, left to be restored by a later undone start"
        );
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// A start killed after it recorded itself started, its container made
    /// and its gate never opened (stage 9, step 9.9c, R2): the next pass
    /// puts back what its last start left (kept aside as
    /// `previous-start.json`), so it reads as exited with its code, and a
    /// boot brings it back. Faked: a start killed at its gate, then its
    /// record and the kept file as the later kill leaves them.
    fn a_start_killed_after_recording_itself_keeps_its_history() {
        let stores = Stores::new();
        let engine = stores.engine();
        let id = Stores::id(&stores.launch(&[
            "--network",
            "none",
            stores.image.as_str(),
            "sh",
            "-c",
            "exit 3",
        ]));
        eventually("its first end, recorded", || {
            matches!(
                stores.state(&id),
                ContainerState::Exited { code: Some(3), .. }
            ) && stores.running_file(&id).try_lock().is_ok()
        });
        let first = stores.record(&id);
        let killed = stores
            .container_command("start-abort", id.as_str())
            .output()
            .unwrap();
        assert!(!killed.status.success(), "{killed:?}");
        assert!(matches!(
            runtime::status(&stores.runtime_root(), id.as_str()).unwrap(),
            Some(runtime::Status::Created | runtime::Status::Creating)
        ));
        let kept = serde_json::json!({
            "version": 1,
            "started": first.started,
            "finished": first.finished,
            "exit": first.exit,
            "boot_id": first.boot_id,
        });
        let dir = containers::testing::record_dir(&engine, &id).unwrap();
        std::fs::write(
            dir.join("previous-start.json"),
            serde_json::to_vec(&kept).unwrap(),
        )
        .unwrap();
        containers::testing::update(&engine, &id, |record| {
            record.started = Some(SystemTime::now());
            record.finished = None;
            record.exit = None;
        })
        .unwrap();
        // The next pass (any container-changing command's).
        let other =
            shipyard_engine::run::create(&engine, stores.request(&["true"]), false).unwrap();
        shipyard_engine::run::remove(&engine, other.id.as_str(), false).unwrap();
        assert!(
            runtime::status(&stores.runtime_root(), id.as_str())
                .unwrap()
                .is_none()
        );
        let record = stores.record(&id);
        assert_eq!(record.started, first.started, "its last start");
        assert_eq!(record.finished, first.finished);
        assert_eq!(record.exit, first.exit, "its last end");
        assert_eq!(record.boot_id, first.boot_id);
        assert!(!dir.join("previous-start.json").exists(), "left");
        assert!(matches!(
            stores.state(&id),
            ContainerState::Exited { code: Some(3), .. }
        ));
        // A policy that brings it back at boot does.
        shipyard_engine::run::update(
            &engine,
            id.as_str(),
            &shipyard_engine::run::UpdateRequest::restart(
                shipyard_engine::containers::RestartPolicy::OnFailure { max: Some(1) },
            ),
        )
        .unwrap();
        let booted = shipyard_engine::run::boot(&engine).unwrap();
        assert_eq!(booted.started.len(), 1, "{:?}", booted.failed);
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// A container whose monitor couldn't start its exit command (its
    /// marker, `exit-command-failed`, in its runtime state): settled by the
    /// next pass, which says its restart policy wasn't applied (stage 9,
    /// step 9.9c, R5). Faked: its monitor killed, the marker left.
    fn an_exit_command_that_never_started_is_said() {
        let stores = Stores::new();
        let id = Stores::id(&stores.launch(&[
            "--network",
            "none",
            stores.image.as_str(),
            "sleep",
            "300",
        ]));
        stores.kill_monitor(&id);
        std::fs::write(
            stores
                .runtime_root()
                .join(id.as_str())
                .join("exit-command-failed"),
            b"",
        )
        .unwrap();
        let (engine, seen) = stores.engine_collecting();
        let other =
            shipyard_engine::run::create(&engine, stores.request(&["true"]), false).unwrap();
        assert!(stores.record(&id).finished.is_some(), "settled");
        assert!(
            seen.warnings().iter().any(|w| {
                w.kind == WarningKind::LeftForLater
                    && w.message.contains("exit command couldn't start")
            }),
            "{:?}",
            seen.warnings()
        );
        shipyard_engine::run::remove(&engine, other.id.as_str(), false).unwrap();
        shipyard_engine::run::remove(&engine, id.as_str(), false).unwrap();
    }

    /// `rename`, `port` and `port --sockets` fail with the kinds their
    /// docs give (stage 9, step 9.9a's gap tests), each beside a call that
    /// works: a taken name `Exists`, a name that can't be one or its own
    /// `InvalidRequest`, a container that isn't there `NotFound`, the
    /// sockets of one that has ended `NotRunning`; a port it doesn't
    /// publish is no error, just none.
    fn rename_port_and_sockets_refuse_by_kind() {
        let stores = Stores::new();
        let engine = stores.engine();
        let made = |name: &str| {
            let mut request = stores.request(&["sh", "-c", "exit 0"]);
            request.name = Some(name.to_owned());
            shipyard_engine::run::create(&engine, request, false)
                .unwrap()
                .id
        };
        let (a, b) = (made("kinds-a"), made("kinds-b"));
        let kind = |result: shipyard_engine::Result<Record>| result.unwrap_err().kind();
        let rename =
            |reference: &str, new: &str| shipyard_engine::run::rename(&engine, reference, new);
        assert_eq!(kind(rename("kinds-a", "kinds-b")), ErrorKind::Exists);
        assert_eq!(
            kind(rename("kinds-a", "bad name")),
            ErrorKind::InvalidRequest
        );
        assert_eq!(
            kind(rename("kinds-a", "kinds-a")),
            ErrorKind::InvalidRequest
        );
        assert_eq!(kind(rename("nobody", "kinds-c")), ErrorKind::NotFound);
        // The control: a free name, taken.
        assert_eq!(
            rename("kinds-a", "kinds-c").unwrap().name.as_str(),
            "kinds-c"
        );
        assert_eq!(stores.record(&a).name.as_str(), "kinds-c");
        // `port`: a container that isn't there; one without the port.
        assert_eq!(
            shipyard_engine::containers::ports(&engine, "nobody", None)
                .unwrap_err()
                .kind(),
            ErrorKind::NotFound
        );
        let none =
            shipyard_engine::containers::ports(&engine, "kinds-c", Some((80, "tcp"))).unwrap();
        assert_eq!(none.name, "kinds-c");
        assert!(none.ports.is_empty(), "{:?}", none.ports);
        // `port --sockets`: of one that has ended, and of none.
        shipyard_engine::run::start(&engine, a.as_str()).unwrap();
        assert_eq!(shipyard_engine::run::wait(&engine, a.as_str()).unwrap(), 0);
        assert_eq!(
            shipyard_engine::containers::sockets(&engine, "kinds-c")
                .unwrap_err()
                .kind(),
            ErrorKind::NotRunning
        );
        assert_eq!(
            shipyard_engine::containers::sockets(&engine, "nobody")
                .unwrap_err()
                .kind(),
            ErrorKind::NotFound
        );
        for id in [a, b] {
            shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
        }
    }

    /// A `network connect` killed once its record says the network, before
    /// the container joined, is finished by the same `connect` run again
    /// (F57): its record names the network and it has no lease there; the
    /// rerun joins it (a lease). A `connect` asking other settings isn't the
    /// same command: still `Exists`, and joins nothing. `disconnect` alike:
    /// killed once its record no longer says it, its lease still held, the
    /// rerun takes it off. The controls: once finished, each again is
    /// refused as before (`Exists`, `NotFound`). Root (bridge networks).
    fn a_cut_short_connect_is_finished_by_its_rerun() {
        use shipyard_engine::run::{ConnectRequest, connect, disconnect};
        if !is_root() {
            return;
        }
        let stores = Stores::new();
        let engine = stores.engine();
        for name in ["home", "extra"] {
            shipyard_engine::network::create(
                &engine,
                &shipyard_network::store::NetworkSpec {
                    name: name.to_owned(),
                    ..Default::default()
                },
            )
            .unwrap();
        }
        let mut request = stores.request(&["sleep", "300"]);
        request.name = Some("joiner".to_owned());
        request.network = vec![NetworkMode::bridge("home")];
        let id = shipyard_engine::run::create(&engine, request, false)
            .unwrap()
            .id;
        shipyard_engine::run::start(&engine, id.as_str()).unwrap();
        let cut = |command: &str| {
            let mut child = stores.container_command(command, "extra");
            child.arg("joiner");
            let output = child.output().unwrap();
            assert!(
                !output.status.success(),
                "{command} wasn't cut short: {output:?}"
            );
        };
        let on_extra = || {
            stores.record(&id).config.network.iter().any(|network| {
                matches!(network, shipyard_engine::containers::Network::Bridges(bridges)
                if bridges.iter().any(|bridge| bridge.name == "extra"))
            })
        };
        cut("connect-abort");
        assert!(on_extra(), "its record says it");
        assert!(stores.leases_on("extra").is_empty(), "not joined");
        // Another request (other settings) isn't the cut-short one: refused.
        let mut other = ConnectRequest::new("extra", "joiner");
        other.aliases = vec!["elsewhere".to_owned()];
        assert_eq!(
            connect(&engine, &other).unwrap_err().kind(),
            ErrorKind::Exists
        );
        assert!(stores.leases_on("extra").is_empty(), "nor joined by it");
        connect(&engine, &ConnectRequest::new("extra", "joiner")).unwrap();
        assert_eq!(stores.leases_on("extra").len(), 1, "joined by the rerun");
        // Joined, the same again is refused as before (the control).
        assert_eq!(
            connect(&engine, &ConnectRequest::new("extra", "joiner"))
                .unwrap_err()
                .kind(),
            ErrorKind::Exists
        );
        cut("disconnect-abort");
        assert!(!on_extra(), "its record no longer says it");
        assert_eq!(stores.leases_on("extra").len(), 1, "not left");
        disconnect(&engine, "extra", "joiner", false).unwrap();
        assert!(stores.leases_on("extra").is_empty(), "left by the rerun");
        // Done, the same again is refused as before (the control).
        assert_eq!(
            disconnect(&engine, "extra", "joiner", false)
                .unwrap_err()
                .kind(),
            ErrorKind::NotFound
        );
        shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
    }

    /// `network connect` and `disconnect` fail with the kinds their docs
    /// give (stage 9, step 9.9a's gap tests). A normal user: `NeedsRoot`
    /// (bridge networks are root's). Root (in the tests' own network
    /// namespace), on stopped containers (their records only), each beside
    /// a call that works: not a bridge network, or a container whose network
    /// isn't bridges, `InvalidRequest`; no such network or container
    /// `NotFound`; on it already `Exists`; not on it `NotFound`; its only
    /// network `InvalidRequest`.
    fn connect_and_disconnect_refuse_by_kind() {
        use shipyard_engine::run::{ConnectRequest, connect, disconnect};
        let stores = Stores::new();
        let engine = stores.engine();
        if !is_root() {
            let mut request = stores.request(&["true"]);
            request.name = Some("plain".to_owned());
            let id = shipyard_engine::run::create(&engine, request, false)
                .unwrap()
                .id;
            assert_eq!(
                connect(&engine, &ConnectRequest::new("kinds", "plain"))
                    .unwrap_err()
                    .kind(),
                ErrorKind::NeedsRoot
            );
            assert_eq!(
                disconnect(&engine, "kinds", "plain", false)
                    .unwrap_err()
                    .kind(),
                ErrorKind::NeedsRoot
            );
            shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
            return;
        }
        for name in ["kinds", "spare"] {
            shipyard_engine::network::create(
                &engine,
                &shipyard_network::store::NetworkSpec {
                    name: name.to_owned(),
                    ..Default::default()
                },
            )
            .unwrap();
        }
        let made = |name: &str, network: NetworkMode| {
            let mut request = stores.request(&["true"]);
            request.name = Some(name.to_owned());
            request.network = vec![network];
            shipyard_engine::run::create(&engine, request, false)
                .unwrap()
                .id
        };
        let web = made("web", NetworkMode::bridge("kinds"));
        let plain = made("plain", NetworkMode::None);
        let connected = |network: &str, container: &str| {
            connect(&engine, &ConnectRequest::new(network, container))
        };
        let kind = |result: shipyard_engine::Result<()>| result.unwrap_err().kind();
        assert_eq!(kind(connected("host", "web")), ErrorKind::InvalidRequest);
        assert_eq!(kind(connected("nonet", "web")), ErrorKind::NotFound);
        assert_eq!(kind(connected("spare", "nobody")), ErrorKind::NotFound);
        assert_eq!(kind(connected("spare", "plain")), ErrorKind::InvalidRequest);
        assert_eq!(kind(connected("kinds", "web")), ErrorKind::Exists);
        // The control: a network it isn't on, joined (its record).
        connected("spare", "web").unwrap();
        assert_eq!(kind(connected("spare", "web")), ErrorKind::Exists);
        // Leaving: one it's on, then the same again, then its last.
        disconnect(&engine, "spare", "web", false).unwrap();
        assert_eq!(
            kind(disconnect(&engine, "spare", "web", false)),
            ErrorKind::NotFound
        );
        assert_eq!(
            kind(disconnect(&engine, "kinds", "web", false)),
            ErrorKind::InvalidRequest
        );
        assert_eq!(
            kind(disconnect(&engine, "host", "web", false)),
            ErrorKind::InvalidRequest
        );
        assert_eq!(
            kind(disconnect(&engine, "kinds", "nobody", false)),
            ErrorKind::NotFound
        );
        for id in [web, plain] {
            shipyard_engine::run::remove(&engine, id.as_str(), true).unwrap();
        }
        for name in ["kinds", "spare"] {
            shipyard_engine::network::remove(&engine, name).unwrap();
        }
    }
}
