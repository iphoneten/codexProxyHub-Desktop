//! 独立的进程内运行日志，不写入请求用量数据库或磁盘。
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::OnceLock;

const CAPACITY: usize = 1000;

#[derive(Clone)]
pub(crate) struct Entry {
    pub time: String,
    pub first_time: String,
    pub level: &'static str,
    pub message: String,
    pub repeat_count: u64,
    group: Option<String>,
}

fn entries() -> &'static Mutex<VecDeque<Entry>> {
    static ENTRIES: OnceLock<Mutex<VecDeque<Entry>>> = OnceLock::new();
    ENTRIES.get_or_init(|| Mutex::new(VecDeque::with_capacity(CAPACITY)))
}

/// 调用方只传运行状态；禁止传入凭据、请求体或原始上游错误内容。
pub(crate) fn record(level: &'static str, message: impl Into<String>) {
    let time = chrono::Local::now().format("%H:%M:%S").to_string();
    push(&mut entries().lock(), Entry {
        first_time: time.clone(),
        time,
        level,
        message: message.into(),
        repeat_count: 1,
        group: None,
    });
}

/// 只合并带内部请求范围哈希的事件；不同请求复用外部 request_id 也不会混合。
pub(crate) fn record_repeated(level: &'static str, message: impl Into<String>, group: String) {
    let time = chrono::Local::now().format("%H:%M:%S").to_string();
    let mut log = entries().lock();
    if let Some(index) = log.iter().position(|entry| entry.level == level && entry.group.as_ref() == Some(&group)) {
        let mut entry = log.remove(index).expect("index belongs to log");
        entry.repeat_count = entry.repeat_count.saturating_add(1);
        entry.time = time;
        entry.message = message.into();
        log.push_back(entry);
    } else {
        push(&mut log, Entry {
            first_time: time.clone(), time, level, message: message.into(), repeat_count: 1, group: Some(group),
        });
    }
}

fn push(entries: &mut VecDeque<Entry>, entry: Entry) {
    while entries.len() >= CAPACITY {
        entries.pop_front();
    }
    entries.push_back(entry);
}

pub(crate) fn snapshot() -> Vec<Entry> {
    entries().lock().iter().rev().cloned().collect()
}

pub(crate) fn clear() {
    entries().lock().clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_log_keeps_latest_entries() {
        let mut entries = VecDeque::new();
        for n in 0..CAPACITY + 5 {
            push(&mut entries, Entry {
                time: String::new(), first_time: String::new(), level: "INFO", message: n.to_string(),
                repeat_count: 1, group: None,
            });
        }
        assert_eq!(entries.len(), CAPACITY);
        assert_eq!(entries.front().unwrap().message, "5");
        assert_eq!(entries.back().unwrap().message, (CAPACITY + 4).to_string());
    }

    #[test]
    fn repeated_events_merge_without_mixing_different_scopes() {
        let group = uuid::Uuid::new_v4().to_string();
        for _ in 0..7 { record_repeated("WARN", "synthetic repeated error", group.clone()); }
        record_repeated("WARN", "synthetic repeated error", format!("{group}-other"));
        let log = snapshot();
        let matching = log.iter().filter(|entry| entry.group.as_ref() == Some(&group)).collect::<Vec<_>>();
        assert_eq!(matching.len(), 1);
        assert_eq!(matching[0].repeat_count, 7);
        assert!(!matching[0].first_time.is_empty());
        assert!(log.iter().any(|entry| entry.group.as_deref() == Some(&format!("{group}-other")) && entry.repeat_count == 1));
    }
}
