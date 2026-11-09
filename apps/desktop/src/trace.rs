//! `SYNT_TRACE_UI=1`: one stderr line per timed phase of a workspace/session switch (store
//! reads, resume replay, engine launch, the frame snapshot). Off, a span costs one `Option`.

use std::time::Instant;

use synthesis_config::Env;

pub struct Span(Option<(&'static str, Instant)>);

pub fn span(env: &Env, name: &'static str) -> Span {
    Span(
        env.get_nonempty("SYNT_TRACE_UI")
            .map(|_| (name, Instant::now())),
    )
}

impl Drop for Span {
    fn drop(&mut self) {
        if let Some((n, t)) = self.0 {
            eprintln!("synthesis-desktop: trace: {n} {:?}", t.elapsed());
        }
    }
}
