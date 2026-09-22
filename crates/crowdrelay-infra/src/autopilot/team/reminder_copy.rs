// The digest tail — the line that tells a recipient what else of theirs is
// waiting, appended under the task that carried the e-mail.
//
// Included into `team.rs` rather than compiled as its own module: split out to
// keep that file under the source-size ratchet, and because it reads the
// file's own private types.
//
// This file used to hold the reminder lane's title and detail copy too. The
// lane is retired — the daily briefing is the cadence — so only the tail the
// first-notice digest still uses remains.

/// The line that tells a recipient what else of theirs is waiting.
///
/// The digest is the fix for what the inbox actually looked like: four
/// approvals of the same kind produced four emails in the same minute with
/// byte-identical subjects, because the subject comes from the action kind and
/// nothing else. A person cannot tell those apart, cannot tell whether they are
/// duplicates of one task, and learns within a day to ignore all of them. One
/// email that names the others is the same information and one interruption.
fn digest_tail(others: &[String], locale: BriefingLocale) -> String {
    // The list is bounded because the e-mail is: the n8n workflow slices
    // task_detail at 1800 chars, and a tail that fills it alone would cut
    // the named tasks mid-item — the digest's whole point — plus drop the
    // closing line. Twenty-four is what used to be the reminder sweep's own
    // batch size, so a tail this long already reads like a digest, not a list.
    const MAX_SHOWN: usize = 24;
    if others.is_empty() {
        return String::new();
    }
    let shown = others.len().min(MAX_SHOWN);
    let header = match (locale, others.len()) {
        (BriefingLocale::Pl, 1) => "Czeka na Ciebie jeszcze jedno zadanie:".to_owned(),
        (BriefingLocale::Pl, n) => format!("Czekają na Ciebie jeszcze {n} zadania:"),
        (BriefingLocale::En, 1) => "One more task is waiting for you:".to_owned(),
        (BriefingLocale::En, n) => format!("{n} more tasks are waiting for you:"),
    };
    let mut tail = format!("\n\n{header}");
    for other in others.iter().take(shown) {
        tail.push_str(&format!("\n• {other}"));
    }
    if shown < others.len() {
        let folded = others.len() - shown;
        let more = match locale {
            BriefingLocale::Pl => format!("\n• … i {folded} więcej"),
            BriefingLocale::En => format!("\n• … and {folded} more"),
        };
        tail.push_str(&more);
    }
    let closing = match locale {
        BriefingLocale::Pl => "\n\nWszystkie są w panelu operacyjnym pod tym samym linkiem.",
        BriefingLocale::En => "\n\nAll of them are in the operations panel behind the same link.",
    };
    tail.push_str(closing);
    tail
}
