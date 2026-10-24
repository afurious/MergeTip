//! `bot_comments`: the one collapsed MergeTip status comment per thread.

use sqlx::PgConnection;

use crate::error::StoreResult;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BotComment {
    pub github_comment_id: i64,
    pub lines: Vec<String>,
}

/// Serialises writers of one thread's status comment until the caller's transaction ends.
/// Without it, two concurrent updates would each append to the same old body and one line would
/// be lost.
pub async fn lock_thread(
    conn: &mut PgConnection,
    repo: &str,
    thread_number: u32,
) -> StoreResult<()> {
    let key = format!("bot_comment:{repo}#{thread_number}");
    sqlx::query!("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))", key)
        .execute(conn)
        .await?;
    Ok(())
}

pub async fn get(
    conn: &mut PgConnection,
    repo: &str,
    thread_number: u32,
) -> StoreResult<Option<BotComment>> {
    let number = i32::try_from(thread_number).map_err(crate::error::corrupt)?;
    let row = sqlx::query_as!(
        BotComment,
        r#"
        SELECT github_comment_id, lines FROM bot_comments
        WHERE repo = $1 AND thread_number = $2
        "#,
        repo,
        number,
    )
    .fetch_optional(conn)
    .await?;
    Ok(row)
}

pub async fn upsert(
    conn: &mut PgConnection,
    repo: &str,
    thread_number: u32,
    comment: &BotComment,
) -> StoreResult<()> {
    let number = i32::try_from(thread_number).map_err(crate::error::corrupt)?;
    sqlx::query!(
        r#"
        INSERT INTO bot_comments (repo, thread_number, github_comment_id, lines)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (repo, thread_number) DO UPDATE SET
            github_comment_id = EXCLUDED.github_comment_id,
            lines = EXCLUDED.lines
        "#,
        repo,
        number,
        comment.github_comment_id,
        &comment.lines,
    )
    .execute(conn)
    .await?;
    Ok(())
}
