//! Review comments: the in-memory store the diff pane writes into, drained
//! by the flush cycle.

use super::*;

impl AppState {
    pub fn review_comments(&self, key: &str) -> &[ReviewComment] {
        self.review_comments
            .get(key)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    pub fn add_review_comment(&mut self, key: &str, comment: ReviewComment) {
        self.review_comments
            .entry(key.to_string())
            .or_default()
            .push(comment);
    }

    pub fn remove_review_comment(&mut self, key: &str, id: &str) {
        if let Some(list) = self.review_comments.get_mut(key) {
            list.retain(|c| c.id != id);
            if list.is_empty() {
                self.review_comments.remove(key);
                self.review_comment_flushes.remove(key);
            }
        }
    }

    pub fn update_review_comment_line(&mut self, key: &str, id: &str, line: u32) {
        if let Some(comment) = self
            .review_comments
            .get_mut(key)
            .and_then(|comments| comments.iter_mut().find(|comment| comment.id == id))
        {
            comment.line = line;
        }
    }

    pub fn rename_review_comment_path(&mut self, key: &str, old_path: &str, new_path: &str) {
        if let Some(comments) = self.review_comments.get_mut(key) {
            for comment in comments
                .iter_mut()
                .filter(|comment| comment.is_file() && comment.path == old_path)
            {
                comment.path = new_path.to_string();
            }
        }
    }

    pub fn take_review_comments(&mut self, key: &str) -> Vec<ReviewComment> {
        self.review_comment_flushes.remove(key);
        self.review_comments.remove(key).unwrap_or_default()
    }

    pub fn purge_review_comments(&mut self, key: &str) {
        self.review_comment_flushes.remove(key);
        self.review_comments.remove(key);
    }

    pub fn begin_review_comment_flush(&mut self, key: &str, source: u64) {
        self.review_comment_flushes
            .entry(key.to_string())
            .or_default()
            .insert(source);
    }

    pub fn finish_review_comment_flush(&mut self, key: &str, source: u64) {
        let remove_key = self
            .review_comment_flushes
            .get_mut(key)
            .is_some_and(|sources| {
                sources.remove(&source);
                sources.is_empty()
            });
        if remove_key {
            self.review_comment_flushes.remove(key);
        }
    }

    pub fn review_comment_flush_pending(&self, key: &str) -> bool {
        self.review_comment_flushes.contains_key(key)
    }
}
