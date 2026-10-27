// An amd64 program for the Rosetta probe: threads (musl's clone), and whether two system
// calls Rosetta has lacked answer (EINVAL/EBADF) or are missing (ENOSYS).
use std::arch::asm;

fn raw(number: i64, a: i64, b: i64) -> i64 {
    let ret: i64;
    // SAFETY: a system call with arguments that make it fail before it touches memory.
    unsafe { asm!("syscall", inlateout("rax") number => ret, in("rdi") a, in("rsi") b, out("rcx") _, out("r11") _) };
    ret
}

fn main() {
    let handles: Vec<_> = (0..8).map(|n| std::thread::spawn(move || (0..1_000_000u64).map(|i| i ^ n).sum::<u64>())).collect();
    let total: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
    println!("8 threads ok ({total})");
    let name = |r: i64| match -r { 22 => "EINVAL (there)".to_owned(), 9 => "EBADF (there)".to_owned(), 38 => "ENOSYS (missing)".to_owned(), e => format!("errno {e}") };
    println!("clone3: {}", name(raw(435, 0, 0)));
    println!("execveat: {}", name(raw(322, -1, 0)));
}
