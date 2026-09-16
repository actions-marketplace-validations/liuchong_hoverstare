//! PR self-driving queue (spec 11 §6): task queue, round claim, artifact gate.
//!
//! Pure logic, no I/O. All state lives on the GitHub side in an append-only
//! hidden marker `<!-- hoverstare-queue:{json} -->`; the canonical state is the
//! **latest** marker (the same read path as the round marker in
//! [`crate::devagent`]). Instruction text is never duplicated into the marker:
//! an item stores the id of the comment that carries the instruction.
//!
//! Strict serialization has two halves. The workflow half is the `concurrency`
//! group of a develop-capable event: it must *queue*, never cancel, so at most
//! one dev round runs at a time. This module is the crash-safe half:
//! [`precheck`] / [`plan_round`] refuse to start a round that a newer run has
//! already finished (claim guard) or whose predecessor did not land on the
//! branch (failure-stop + ancestor gate), so a duplicated or superseded run
//! writes nothing at all.

use serde::{Deserialize, Serialize};

use crate::github::IssueComment;

/// Hidden queue-state marker (append-only; the latest one wins).
pub const QUEUE_PREFIX: &str = "<!-- hoverstare-queue:";
/// Max *open* (pending/running) items; further instructions are rejected aloud.
pub const MAX_ITEMS: usize = 20;
/// Max characters of one instruction (longer ones are rejected aloud).
pub const MAX_TEXT: usize = 2000;
/// Terminal items kept as history so a finished queue does not eat the cap.
const MAX_HISTORY: usize = 10;
/// Queue schema version.
const QUEUE_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Queue state
// ---------------------------------------------------------------------------

/// Who created a queued item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ItemKind {
    /// A human `@hoverstare <instruction>` comment.
    #[default]
    Human,
    /// Work the bot queued for itself (self-trigger): always behind humans.
    Bot,
}

impl ItemKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ItemKind::Human => "human",
            ItemKind::Bot => "bot",
        }
    }

    /// Unknown values read back as `Human` (conservative: never lower priority).
    pub fn parse(s: &str) -> Self {
        if s.eq_ignore_ascii_case("bot") {
            ItemKind::Bot
        } else {
            ItemKind::Human
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ItemKind::Human => "人类",
            ItemKind::Bot => "自触发",
        }
    }
}

impl Serialize for ItemKind {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ItemKind {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(ItemKind::parse(&String::deserialize(d)?))
    }
}

/// Lifecycle of one queued item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ItemState {
    /// Waiting its turn.
    #[default]
    Pending,
    /// In flight: a self-triggered continuation of the same instruction.
    Running,
    /// Landed on the branch.
    Done,
    /// The round made no progress; never retried automatically.
    Failed,
    /// Discarded by a human (`@hoverstare merge force`).
    Dropped,
}

impl ItemState {
    pub fn as_str(self) -> &'static str {
        match self {
            ItemState::Pending => "pending",
            ItemState::Running => "running",
            ItemState::Done => "done",
            ItemState::Failed => "failed",
            ItemState::Dropped => "dropped",
        }
    }

    /// Unknown values read back as `Pending` (conservative: keep working on it).
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "running" => ItemState::Running,
            "done" => ItemState::Done,
            "failed" => ItemState::Failed,
            "dropped" => ItemState::Dropped,
            _ => ItemState::Pending,
        }
    }

    /// Still to be worked on (eligible for dequeue).
    pub fn is_open(self) -> bool {
        matches!(self, ItemState::Pending | ItemState::Running)
    }

    /// Kept only as history.
    pub fn is_terminal(self) -> bool {
        !self.is_open()
    }

    pub fn checkbox(self) -> &'static str {
        match self {
            ItemState::Pending => "[ ]",
            ItemState::Running => "[~]",
            ItemState::Done => "[x]",
            ItemState::Failed => "[!]",
            ItemState::Dropped => "[-]",
        }
    }
}

impl Serialize for ItemState {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ItemState {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(ItemState::parse(&String::deserialize(d)?))
    }
}

/// One queued instruction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    /// Id of the source comment: the instruction text lives there.
    pub src: u64,
    #[serde(default)]
    pub kind: ItemKind,
    #[serde(default)]
    pub state: ItemState,
}

/// The queue as stored in the marker. Items are in arrival order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueState {
    #[serde(default = "queue_version")]
    pub v: u32,
    #[serde(default)]
    pub items: Vec<Item>,
}

fn queue_version() -> u32 {
    QUEUE_VERSION
}

impl Default for QueueState {
    fn default() -> Self {
        Self::new()
    }
}

impl QueueState {
    pub fn new() -> Self {
        Self {
            v: QUEUE_VERSION,
            items: Vec::new(),
        }
    }

    /// Parse the (last) queue marker of one comment body.
    pub fn parse(body: &str) -> Option<Self> {
        let start = body.rfind(QUEUE_PREFIX)? + QUEUE_PREFIX.len();
        let end = body[start..].find("-->")? + start;
        serde_json::from_str(body[start..end].trim()).ok()
    }

    /// Latest queue state in a comment thread (append-only marker, latest wins).
    pub fn latest(comments: &[IssueComment]) -> Option<Self> {
        comments
            .iter()
            .rev()
            .filter_map(|c| c.body.as_deref())
            .find_map(Self::parse)
    }

    pub fn render(&self) -> String {
        format!(
            "{QUEUE_PREFIX}{} -->",
            serde_json::to_string(self).expect("queue state is serializable")
        )
    }

    /// Append one instruction. Idempotent per source comment, and loudly
    /// refuses (never silently drops) when a cap would be exceeded.
    pub fn enqueue(&mut self, src: u64, kind: ItemKind, text: &str) -> Result<(), QueueError> {
        if self.items.iter().any(|i| i.src == src) {
            return Ok(()); // the same comment triggers at most one item
        }
        if self.open_count() >= MAX_ITEMS {
            return Err(QueueError::Full);
        }
        if text.chars().count() > MAX_TEXT {
            return Err(QueueError::TooLong);
        }
        self.prune_history();
        self.items.push(Item {
            src,
            kind,
            state: ItemState::Pending,
        });
        Ok(())
    }

    pub fn set_state(&mut self, src: u64, state: ItemState) {
        if let Some(item) = self.items.iter_mut().find(|i| i.src == src) {
            item.state = state;
        }
    }

    /// Items still to be worked on (drives `@hoverstare merge` refusal).
    pub fn outstanding(&self) -> Vec<&Item> {
        self.items.iter().filter(|i| i.state.is_open()).collect()
    }

    pub fn open_count(&self) -> usize {
        self.items.iter().filter(|i| i.state.is_open()).count()
    }

    /// Dequeue order: the in-flight item first (self-triggered continuation),
    /// then human instructions by ascending id (FIFO = dependency order), then
    /// bot items. One item per round.
    pub fn ordered(&self) -> Vec<&Item> {
        let mut open: Vec<&Item> = self.items.iter().filter(|i| i.state.is_open()).collect();
        open.sort_by_key(|i| {
            (
                i.state != ItemState::Running,
                i.kind != ItemKind::Human,
                i.src,
            )
        });
        open
    }

    /// The item the next round would work on.
    pub fn next(&self) -> Option<&Item> {
        self.ordered().into_iter().next()
    }

    /// Drop the oldest terminal items so the live cap is not eaten by history.
    fn prune_history(&mut self) {
        let terminal = self.items.iter().filter(|i| i.state.is_terminal()).count();
        if terminal <= MAX_HISTORY {
            return;
        }
        let mut drop = terminal - MAX_HISTORY;
        self.items.retain(|i| {
            if drop > 0 && i.state.is_terminal() {
                drop -= 1;
                false
            } else {
                true
            }
        });
    }
}

/// Why an instruction was not queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueError {
    Full,
    TooLong,
}

impl QueueError {
    /// Human-visible refusal (never a silent drop).
    pub fn message(self) -> String {
        match self {
            QueueError::Full => format!(
                "⚠️ 任务队列已满（未完成任务上限 {MAX_ITEMS} 条），本条指令未入队。\
                 请等待队列消化，或让人类清理队列后再下指令。"
            ),
            QueueError::TooLong => {
                format!("⚠️ 指令过长（上限 {MAX_TEXT} 字符），本条未入队；请拆成更小的任务。")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Round planning (decide half; devagent::pr_dev_round is the act half)
// ---------------------------------------------------------------------------

/// How one round ended, as recorded in the round marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Changes committed and pushed.
    Ok,
    /// No change: no progress, so self-driving stops.
    Nochange,
    /// The round failed.
    Failed,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Nochange => "nochange",
            Outcome::Failed => "failed",
        }
    }

    /// Unknown/absent values are `None` and treated as "not ok" (stop).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ok" => Some(Outcome::Ok),
            "nochange" => Some(Outcome::Nochange),
            "failed" => Some(Outcome::Failed),
            _ => None,
        }
    }
}

/// The queue-relevant part of the latest round marker.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoundRecord {
    /// Rounds completed so far.
    pub r: u32,
    /// How the last round ended (`None` = absent or unrecognized).
    pub st: Option<Outcome>,
    /// Commit pushed by the last round.
    pub sha: Option<String>,
    /// Queue item the last round worked on (`None` = not a queue round).
    pub task: Option<u64>,
}

/// Why a round has nothing to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Idle {
    /// Round cap reached.
    RoundCap,
    /// A newer run already finished the claimed round: stay completely silent.
    StaleClaim,
    /// The previous round made no progress (failure-stop for self-driving).
    PreviousFailed,
    /// The previous round's commit is no longer reachable from the head.
    PreviousNotOnBranch,
    /// No queued instruction left.
    EmptyQueue,
}

impl Idle {
    /// True when the run must not write anything at all.
    pub fn silent(self) -> bool {
        matches!(self, Idle::StaleClaim)
    }

    pub fn message(self) -> &'static str {
        match self {
            Idle::RoundCap => "已达最大开发轮次上限，自驱动停止，请人类接管。",
            Idle::StaleClaim => "(stale claim; nothing written)",
            Idle::PreviousFailed => {
                "上一轮未成功落地（无进展或未推送），自驱动已暂停；请人类检查后再下指令。"
            }
            Idle::PreviousNotOnBranch => {
                "上一轮记录的提交已不在当前分支上（可能被改写），自驱动已暂停；请人类确认。"
            }
            Idle::EmptyQueue => "任务队列为空，没有待执行的指令。",
        }
    }
}

/// One round's decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoundPlan {
    Run(RunItem),
    Idle(Idle),
}

/// The instruction one round must execute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunItem {
    pub round: u32,
    /// Source comment id (`0` = direct instruction without a source comment).
    pub src: u64,
    pub kind: ItemKind,
    pub text: String,
}

/// Everything the decision needs; all of it is pure data.
pub struct RoundRequest<'a> {
    /// Round this run would execute (`latest.r + 1`).
    pub round: u32,
    pub max_rounds: u32,
    /// Round claimed by the triggering comment (self-trigger only).
    pub claim: Option<u32>,
    pub latest: Option<&'a RoundRecord>,
    /// Latest queue state, with the triggering instruction already enqueued.
    pub queue: &'a QueueState,
    /// Whether `latest.sha` is an ancestor of the current branch head
    /// (`git merge-base --is-ancestor <sha> <head>`; ignored when no sha).
    pub recorded_on_branch: bool,
    /// PR comments: instruction text is read back from `src`.
    pub comments: &'a [IssueComment],
    /// Instruction without a source comment (PR review body): runs immediately,
    /// outside the queue (it cannot be addressed by comment id).
    pub direct: Option<&'a str>,
}

/// Guards that need no repository access: round cap and the self-driving claim.
pub fn precheck(
    round: u32,
    max_rounds: u32,
    claim: Option<u32>,
    latest: Option<&RoundRecord>,
) -> Option<Idle> {
    // The claim guard turns the lossy "two runs race" into last-writer-wins:
    // if the newest completed round already reached the claimed one, this run
    // is a leftover and must not touch anything. It is checked *before* the cap
    // so a superseded run stays silent even once the round limit is reached.
    if let (Some(claim), Some(latest)) = (claim, latest)
        && latest.r >= claim
    {
        return Some(Idle::StaleClaim);
    }
    if round > max_rounds {
        return Some(Idle::RoundCap);
    }
    None
}

/// Decide what (if anything) this round must do.
pub fn plan_round(req: &RoundRequest<'_>) -> RoundPlan {
    if let Some(idle) = precheck(req.round, req.max_rounds, req.claim, req.latest) {
        return RoundPlan::Idle(idle);
    }

    // Failure-stop + artifact gate. Only a *self-driving* continuation has to
    // prove that the previous task landed; a human command always gets to run
    // (otherwise a failed round would lock the PR forever).
    if req.claim.is_some()
        && let Some(latest) = req.latest
        && latest.task.is_some()
    {
        if latest.st != Some(Outcome::Ok) {
            return RoundPlan::Idle(Idle::PreviousFailed);
        }
        if latest.sha.is_none() || !req.recorded_on_branch {
            return RoundPlan::Idle(Idle::PreviousNotOnBranch);
        }
    }

    if let Some(text) = req.direct.filter(|t| !t.trim().is_empty()) {
        return RoundPlan::Run(RunItem {
            round: req.round,
            src: 0,
            kind: ItemKind::Human,
            text: text.to_string(),
        });
    }

    for item in req.queue.ordered() {
        if let Some(text) = instruction(req.comments, item.src) {
            return RoundPlan::Run(RunItem {
                round: req.round,
                src: item.src,
                kind: item.kind,
                text,
            });
        }
    }
    RoundPlan::Idle(Idle::EmptyQueue)
}

/// Instruction text of a queued item, read back from its source comment.
pub fn instruction(comments: &[IssueComment], src: u64) -> Option<String> {
    let body = comments.iter().find(|c| c.id == src)?.body.as_deref()?;
    match crate::devagent::parse_dev_command(body)? {
        crate::devagent::DevCommand::Task(text) if !text.trim().is_empty() => {
            Some(text.trim().to_string())
        }
        _ => None,
    }
}

/// How the item a round just ran ends up (spec 11 §6). `Done` means the task
/// finished, not merely that a commit landed: a round cut short by the tool
/// budget leaves the item pending, which is exactly what lets the next automatic
/// round pick the same instruction up again (`dequeue` re-reads the comment).
/// Without this, every round drained its own item and the automatic chain could
/// never continue at all.
pub fn state_after_round(progressed: bool, budget_exhausted: bool) -> ItemState {
    match (progressed, budget_exhausted) {
        // The budget ran out: the task is unfinished whatever the round managed
        // to commit, so the item stays queued and the next round resumes the
        // same instruction (spec 11 §6).
        (_, true) => ItemState::Pending,
        (true, false) => ItemState::Done,
        (false, false) => ItemState::Failed,
    }
}

/// Whether a finished round pulls the next one (spec 11 §6): only progress that
/// landed, only while the cap allows and only while work remains. A no-change
/// round therefore stops in front of the human instead of looping.
pub fn self_trigger(
    round: u32,
    max_rounds: u32,
    outcome: Outcome,
    budget_exhausted: bool,
    queue: &QueueState,
) -> bool {
    unfinished(outcome, budget_exhausted, queue) && round < max_rounds
}

/// Whether work is still outstanding after a round (spec 11 §6): a round cut
/// short by the budget always is (that is the whole point of continuing), and
/// so is a round that landed progress while the queue has more to do. A
/// no-change round that was *not* cut short has nothing left to continue with.
pub fn unfinished(outcome: Outcome, budget_exhausted: bool, queue: &QueueState) -> bool {
    budget_exhausted || (outcome == Outcome::Ok && !queue.ordered().is_empty())
}

/// The item currently in flight (`ItemState::Running`), if any.
pub fn running(queue: &QueueState) -> Option<&Item> {
    queue.items.iter().find(|i| i.state == ItemState::Running)
}

/// Human-readable label of one queued item: `#<src> <first-line snippet>`, falling
/// back to the id alone when the source comment's text is no longer readable.
pub fn item_label(comments: &[IssueComment], src: u64) -> String {
    match instruction(comments, src) {
        Some(text) => format!("#{src} {}", snippet(&text, 80)),
        None => format!("#{src}"),
    }
}

/// One-line queue status appended to every round report. When an item is in
/// flight the line names it (`正在跑：#<src> <first line>`), so a human can tell
/// *which* task runs and not just how many.
pub fn summary_line(queue: &QueueState, comments: &[IssueComment]) -> String {
    let pending = count(queue, ItemState::Pending);
    let running_count = count(queue, ItemState::Running);
    let failed = count(queue, ItemState::Failed);
    let tail = match running(queue) {
        Some(item) => format!("；▶︎ 正在跑：{}", item_label(comments, item.src)),
        None => queue
            .next()
            .map(|i| format!("；下一轮 #{}", i.src))
            .unwrap_or_default(),
    };
    format!(
        "📋 队列：{pending} 待执行 / {running_count} 进行中 / {failed} 失败（未完成上限 {MAX_ITEMS} 条）{tail}"
    )
}

/// Round-report note: the item this round executed (id + snippet), then the queue
/// status. The item left `Running` when the round finished, so the report names
/// it explicitly instead of relying on `summary_line` to still find it.
pub fn round_note(queue: &QueueState, comments: &[IssueComment], src: u64) -> String {
    format!(
        "▶︎ 本轮执行 {}\n\n{}",
        item_label(comments, src),
        summary_line(queue, comments)
    )
}

/// Full checklist (for `@hoverstare queue`); `→` marks the next round's item.
pub fn checklist(queue: &QueueState, comments: &[IssueComment]) -> String {
    if queue.items.is_empty() {
        return "（队列为空）".to_string();
    }
    let next = queue.next().map(|i| i.src);
    let mut out = String::new();
    for item in &queue.items {
        let text = instruction(comments, item.src)
            .map(|t| snippet(&t, 80))
            .unwrap_or_else(|| "（原始评论不可见）".to_string());
        let kind = if item.state == ItemState::Running {
            format!("{}（进行中）", item.kind.label())
        } else {
            item.kind.label().to_string()
        };
        let arrow = if next == Some(item.src) { " →" } else { "" };
        out.push_str(&format!(
            "- {} #{} {}：{}{}\n",
            item.state.checkbox(),
            item.src,
            kind,
            text,
            arrow
        ));
    }
    out
}

/// Verdict of the pre-merge queue gate (spec 11 §6). Pure so the rule is
/// testable without I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeGate {
    /// Nothing unfinished: merge may proceed, nothing is discarded.
    Clear,
    /// Unfinished work and no `force`: refuse and show the checklist.
    Blocked,
    /// `force`: merge may proceed after discarding `dropped` unfinished items.
    Forced { dropped: usize },
}

/// Decide whether `@hoverstare merge` may run (spec 11 §6). Refusing is the
/// default; only a human `force` may throw queued instructions away, and the
/// count is handed back so the discard is reported, never silent.
pub fn merge_gate(queue: &QueueState, force: bool) -> MergeGate {
    let open = queue.outstanding().len();
    if open == 0 {
        MergeGate::Clear
    } else if force {
        MergeGate::Forced { dropped: open }
    } else {
        MergeGate::Blocked
    }
}

fn count(queue: &QueueState, state: ItemState) -> usize {
    queue.items.iter().filter(|i| i.state == state).count()
}

fn snippet(text: &str, max: usize) -> String {
    let first = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let mut out: String = first.chars().take(max).collect();
    if first.chars().count() > max {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::PrUser;

    fn comment(id: u64, body: &str) -> IssueComment {
        IssueComment {
            id,
            body: Some(body.to_string()),
            user: PrUser {
                login: "alice".into(),
            },
        }
    }

    fn record(r: u32, st: Option<Outcome>, sha: Option<&str>, task: Option<u64>) -> RoundRecord {
        RoundRecord {
            r,
            st,
            sha: sha.map(String::from),
            task,
        }
    }

    fn request<'a>(
        round: u32,
        claim: Option<u32>,
        latest: Option<&'a RoundRecord>,
        queue: &'a QueueState,
        comments: &'a [IssueComment],
        on_branch: bool,
    ) -> RoundRequest<'a> {
        RoundRequest {
            round,
            max_rounds: 10,
            claim,
            latest,
            queue,
            recorded_on_branch: on_branch,
            comments,
            direct: None,
        }
    }

    #[test]
    fn queue_roundtrips_and_latest_wins() {
        let mut queue = QueueState::new();
        queue.enqueue(10, ItemKind::Human, "add tests").unwrap();
        queue.set_state(10, ItemState::Done);
        let text = queue.render();
        assert!(text.starts_with(QUEUE_PREFIX), "{text}");
        assert_eq!(
            QueueState::parse(&format!("body\n\n{text}")),
            Some(queue.clone())
        );
        assert_eq!(QueueState::parse("no marker here"), None);

        let comments = vec![
            comment(1, &text),
            comment(2, "plain reply"),
            comment(3, &QueueState::new().render()),
        ];
        assert_eq!(QueueState::latest(&comments), Some(QueueState::new()));
        // unknown enum values degrade instead of losing the whole queue
        let odd =
            format!("{QUEUE_PREFIX}{{\"v\":1,\"items\":[{{\"src\":4,\"state\":\"weird\"}}]}} -->");
        let parsed = QueueState::parse(&odd).unwrap();
        assert_eq!(parsed.items[0].state, ItemState::Pending);
        assert_eq!(parsed.items[0].kind, ItemKind::Human);
    }

    #[test]
    fn dequeue_is_running_then_human_then_fifo() {
        let mut queue = QueueState::new();
        queue.items = vec![
            Item {
                src: 5,
                kind: ItemKind::Human,
                state: ItemState::Pending,
            },
            Item {
                src: 3,
                kind: ItemKind::Bot,
                state: ItemState::Pending,
            },
            Item {
                src: 9,
                kind: ItemKind::Human,
                state: ItemState::Pending,
            },
            Item {
                src: 7,
                kind: ItemKind::Human,
                state: ItemState::Done,
            },
        ];
        // humans win over the older bot item; among humans the lowest id is next
        assert_eq!(queue.next().unwrap().src, 5);
        queue.set_state(5, ItemState::Running);
        // an in-flight task is always continued first
        assert_eq!(queue.next().unwrap().src, 5);
        queue.set_state(5, ItemState::Done);
        assert_eq!(queue.next().unwrap().src, 9);
        assert_eq!(queue.ordered().len(), 2, "done items are not candidates");
    }

    #[test]
    fn stale_claim_and_cap_are_prechecked() {
        // The self-trigger comment records the round just finished (`r`), so the
        // run claims the next one: `latest.r` is the round this run would execute
        // minus one, and any claim at or below it is already superseded.
        let latest = record(5, Some(Outcome::Ok), Some("s1"), Some(10));
        assert_eq!(
            precheck(6, 10, Some(5), Some(&latest)),
            Some(Idle::StaleClaim)
        );
        assert_eq!(
            precheck(6, 10, Some(4), Some(&latest)),
            Some(Idle::StaleClaim)
        );
        assert_eq!(precheck(6, 10, Some(6), Some(&latest)), None);
        assert_eq!(precheck(6, 10, None, Some(&latest)), None, "no claim");
        assert_eq!(precheck(11, 10, None, Some(&latest)), Some(Idle::RoundCap));
        // the claim wins over the cap: a superseded run must still stay silent
        assert_eq!(
            precheck(11, 10, Some(5), Some(&latest)),
            Some(Idle::StaleClaim)
        );
        assert!(Idle::StaleClaim.silent());
        assert!(!Idle::EmptyQueue.silent());
    }

    #[test]
    fn artifact_gate_requires_ok_and_ancestor() {
        let mut queue = QueueState::new();
        queue.enqueue(11, ItemKind::Human, "next task").unwrap();
        let comments = vec![comment(11, "@hoverstare next task")];
        let ok = record(1, Some(Outcome::Ok), Some("s1"), Some(10));
        let nochange = record(1, Some(Outcome::Nochange), None, Some(10));

        assert!(matches!(
            plan_round(&request(2, Some(2), Some(&ok), &queue, &comments, true)),
            RoundPlan::Run(_)
        ));
        // human commits on top of the bot commit are fine: reachability, not equality
        assert_eq!(
            plan_round(&request(2, Some(2), Some(&ok), &queue, &comments, false)),
            RoundPlan::Idle(Idle::PreviousNotOnBranch)
        );
        assert_eq!(
            plan_round(&request(
                2,
                Some(2),
                Some(&nochange),
                &queue,
                &comments,
                true
            )),
            RoundPlan::Idle(Idle::PreviousFailed)
        );
        // the gate only guards self-driving: a human command still runs
        assert!(matches!(
            plan_round(&request(2, None, Some(&nochange), &queue, &comments, false)),
            RoundPlan::Run(_)
        ));
    }

    #[test]
    fn plan_dequeues_fifo_and_skips_vanished_comments() {
        let mut queue = QueueState::new();
        queue.enqueue(11, ItemKind::Human, "first").unwrap();
        queue.enqueue(12, ItemKind::Human, "second").unwrap();
        let comments = vec![comment(12, "@hoverstare second")];
        match plan_round(&request(1, None, None, &queue, &comments, true)) {
            RoundPlan::Run(item) => {
                assert_eq!(item.src, 12, "the vanished first item is skipped");
                assert_eq!(item.text, "second");
                assert_eq!(item.round, 1);
            }
            other => panic!("expected a run, got {other:?}"),
        }

        let empty = QueueState::new();
        assert_eq!(
            plan_round(&request(1, None, None, &empty, &[], true)),
            RoundPlan::Idle(Idle::EmptyQueue)
        );
    }

    #[test]
    fn direct_instruction_runs_without_a_queue_item() {
        let queue = QueueState::new();
        let mut req = request(1, None, None, &queue, &[], true);
        req.direct = Some("tighten the loop");
        match plan_round(&req) {
            RoundPlan::Run(item) => {
                assert_eq!(item.src, 0);
                assert_eq!(item.text, "tighten the loop");
            }
            other => panic!("expected a run, got {other:?}"),
        }
    }

    #[test]
    fn state_after_round_keeps_a_budget_cut_item_queued() {
        assert_eq!(state_after_round(true, false), ItemState::Done);
        assert_eq!(
            state_after_round(true, true),
            ItemState::Pending,
            "a round cut short by the budget must stay queued so the chain resumes it"
        );
        assert_eq!(state_after_round(false, false), ItemState::Failed);
        // Exhausted with nothing committed is still unfinished: the next round
        // gets a fresh budget instead of the work being dropped.
        assert_eq!(state_after_round(false, true), ItemState::Pending);
        let mut cut = QueueState::new();
        cut.enqueue(9, ItemKind::Human, "读了一轮还没改").unwrap();
        cut.set_state(9, state_after_round(false, true));
        assert!(self_trigger(1, 10, Outcome::Nochange, true, &cut));
        assert!(!self_trigger(10, 10, Outcome::Nochange, true, &cut));
        // The composition that makes the automatic chain reachable: one queued
        // instruction, a budget-cut round, and the item still open afterwards.
        let mut queue = QueueState::new();
        queue.enqueue(7, ItemKind::Human, "大任务第一段").unwrap();
        queue.set_state(7, ItemState::Running);
        queue.set_state(7, state_after_round(true, true));
        assert!(
            self_trigger(1, 10, Outcome::Ok, false, &queue),
            "the cut round must pull another one for the same instruction"
        );
        // A round that finished the task drains the queue and stops the chain.
        queue.set_state(7, state_after_round(true, false));
        assert!(!self_trigger(1, 10, Outcome::Ok, false, &queue));
    }

    #[test]
    fn self_trigger_only_after_landed_round_with_work_left() {
        let mut queue = QueueState::new();
        queue.enqueue(11, ItemKind::Human, "task").unwrap();
        queue.set_state(11, ItemState::Done);
        assert!(
            !self_trigger(1, 10, Outcome::Ok, false, &queue),
            "queue drained"
        );

        queue.set_state(11, ItemState::Running);
        assert!(
            self_trigger(1, 10, Outcome::Ok, false, &queue),
            "landed progress with work left"
        );
        assert!(
            !self_trigger(1, 10, Outcome::Nochange, false, &queue),
            "no progress and not cut short: stop in front of the human"
        );

        queue.set_state(11, ItemState::Pending);
        queue.enqueue(12, ItemKind::Human, "task 2").unwrap();
        assert!(
            self_trigger(1, 10, Outcome::Ok, false, &queue),
            "next task queued"
        );
        assert!(
            !self_trigger(10, 10, Outcome::Ok, false, &queue),
            "round cap"
        );
    }

    #[test]
    fn enqueue_refuses_beyond_caps_and_is_idempotent() {
        let mut queue = QueueState::new();
        queue.enqueue(1, ItemKind::Human, "a").unwrap();
        queue.enqueue(1, ItemKind::Human, "a").unwrap();
        assert_eq!(queue.items.len(), 1, "same comment enqueues once");

        let mut fresh = QueueState::new();
        assert_eq!(
            fresh.enqueue(1, ItemKind::Human, &"x".repeat(MAX_TEXT + 1)),
            Err(QueueError::TooLong)
        );
        assert!(
            fresh.items.is_empty(),
            "refused instructions are not queued"
        );
        assert!(
            fresh
                .enqueue(1, ItemKind::Human, &"x".repeat(MAX_TEXT))
                .is_ok()
        );

        for i in 0..(MAX_ITEMS as u64 - 1) {
            queue.enqueue(100 + i, ItemKind::Human, "t").unwrap();
        }
        assert_eq!(queue.open_count(), MAX_ITEMS);
        assert_eq!(
            queue.enqueue(999, ItemKind::Human, "t"),
            Err(QueueError::Full)
        );
        assert!(!QueueError::Full.message().is_empty());
        assert!(!QueueError::TooLong.message().is_empty());
    }

    #[test]
    fn history_is_pruned_so_the_live_cap_stays_free() {
        let mut queue = QueueState::new();
        for i in 0..MAX_ITEMS as u64 {
            queue.enqueue(100 + i, ItemKind::Human, "t").unwrap();
        }
        for item in queue.items.iter_mut() {
            item.state = ItemState::Done;
        }
        queue.enqueue(500, ItemKind::Human, "new work").unwrap();
        assert_eq!(queue.items.len(), MAX_HISTORY + 1);
        assert!(queue.items.iter().any(|i| i.src == 500));
        assert_eq!(queue.open_count(), 1);
    }

    #[test]
    fn checklist_shows_running_and_pending() {
        let comments = vec![
            comment(11, "@hoverstare add tests"),
            comment(12, "@hoverstare then update the docs"),
        ];
        let mut queue = QueueState::new();
        queue.enqueue(11, ItemKind::Human, "add tests").unwrap();
        queue
            .enqueue(12, ItemKind::Human, "then update the docs")
            .unwrap();
        queue.set_state(11, ItemState::Running);
        let out = checklist(&queue, &comments);
        assert!(out.contains("[~] #11"), "{out}");
        assert!(out.contains("（进行中）"), "{out}");
        assert!(out.contains("[ ] #12"), "{out}");
        assert!(out.contains("add tests"), "{out}");
        assert!(out.contains("update the docs"), "{out}");
        let summary = summary_line(&queue, &comments);
        assert!(
            summary.contains("进行中") && summary.contains("待执行"),
            "{summary}"
        );
        // Counts alone are not enough: the in-flight item is named.
        assert!(summary.contains("正在跑：#11 add tests"), "{summary}");
        assert_eq!(
            checklist(&QueueState::new(), &comments),
            "（队列为空）".to_string()
        );
    }

    #[test]
    fn summary_names_running_item_and_falls_back_to_next() {
        let comments = vec![
            comment(11, "@hoverstare add tests"),
            comment(12, "@hoverstare then update the docs"),
        ];
        let mut queue = QueueState::new();
        queue.enqueue(11, ItemKind::Human, "add tests").unwrap();
        queue
            .enqueue(12, ItemKind::Human, "then update the docs")
            .unwrap();

        // Nothing running: the summary points at the next item instead.
        let idle = summary_line(&queue, &comments);
        assert!(idle.contains("下一轮 #11"), "{idle}");
        assert!(!idle.contains("正在跑"), "{idle}");

        // In flight: the summary names id + first-line snippet.
        queue.set_state(11, ItemState::Running);
        let busy = summary_line(&queue, &comments);
        assert!(busy.contains("正在跑：#11 add tests"), "{busy}");

        // A vanished source comment still renders the id.
        queue.set_state(11, ItemState::Done);
        queue.set_state(12, ItemState::Running);
        let gone = summary_line(&queue, &[]);
        assert!(gone.contains("正在跑：#12"), "{gone}");
    }

    #[test]
    fn round_note_names_the_executed_item() {
        let comments = vec![comment(11, "@hoverstare add tests")];
        let mut queue = QueueState::new();
        queue.enqueue(11, ItemKind::Human, "add tests").unwrap();
        queue.set_state(11, ItemState::Done);
        let note = round_note(&queue, &comments, 11);
        assert!(note.contains("本轮执行 #11 add tests"), "{note}");
        assert!(note.contains("队列："), "{note}");
    }

    #[test]
    fn merge_gate_refuses_nonempty_queue_unless_forced() {
        let comments = vec![comment(11, "@hoverstare add tests")];
        let mut queue = QueueState::new();
        queue.enqueue(11, ItemKind::Human, "add tests").unwrap();

        // Outstanding work and no `force`: refuse, and the pasted checklist is
        // non-empty so the refusal says what is still left.
        assert_eq!(merge_gate(&queue, false), MergeGate::Blocked);
        assert_ne!(checklist(&queue, &comments), "（队列为空）");
        assert!(!checklist(&queue, &comments).is_empty());

        // `force`: merge proceeds and reports how many items are discarded.
        assert_eq!(merge_gate(&queue, true), MergeGate::Forced { dropped: 1 });

        // Every open item (pending or running) counts toward the discard.
        queue
            .enqueue(12, ItemKind::Human, "then update the docs")
            .unwrap();
        queue.set_state(12, ItemState::Running);
        queue
            .enqueue(13, ItemKind::Human, "and the changelog")
            .unwrap();
        queue.set_state(13, ItemState::Done);
        assert_eq!(merge_gate(&queue, true), MergeGate::Forced { dropped: 2 });

        // Empty queue: unchanged, nothing is dropped either way.
        assert_eq!(merge_gate(&QueueState::new(), false), MergeGate::Clear);
        assert_eq!(merge_gate(&QueueState::new(), true), MergeGate::Clear);
    }
}
