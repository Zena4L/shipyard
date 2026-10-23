//! What a Docker client sends the API (step 11.2c3b): a request's path
//! (its version prefix, `%XX`s, an image's name with `/`s), its query, its
//! filters' JSON, a container's, a network's, a volume's or an exec's create
//! body (and the exec's child's argv), a registry's
//! credentials header. Anything must be an answer or a refusal, never a
//! panic.
#![no_main]

use libfuzzer_sys::fuzz_target;
use shipyard_api::fuzzing;

fuzz_target!(|data: &[u8]| {
    fuzzing::request(data);
});
