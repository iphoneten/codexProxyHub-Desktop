//! 独立的进程内运行日志，不写入请求用量数据库或磁盘。
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::OnceLock;

const CAPACITY: usize = 1000;

#[derive(Clone)]
pub(crate) struct Entry {
    pub time: String,
    pub level: &'static str,
    pub message: String,
}

fn entries() -> &'static Mutex<VecDeque<Entry>> {
    static ENTRIES: OnceLock<Mutex<VecDeque<Entry>>> = OnceLock::new();
    ENTRIES.get_or_init(|| Mutex::new(VecDeque::with_capacity(CAPACITY)))
}

/// 调用方只传运行状态；禁止传入凭据、请求体或原始上游错误内容。
pub(crate) fn record(level: &'static str, message: impl Into<String>) {
    push(&mut entries().lock(), Entry {
        time: chrono::Local::now().format("%H:%M:%S").to_string(),
        level,
        message: message.into(),
    });
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
                time: String::new(), level: "INFO", message: n.to_string(),
            });
        }
        assert_eq!(entries.len(), CAPACITY);
        assert_eq!(entries.front().unwrap().message, "5");
        assert_eq!(entries.back().unwrap().message, (CAPACITY + 4).to_string());
    }
}
