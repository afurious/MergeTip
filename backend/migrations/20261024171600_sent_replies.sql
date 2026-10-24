-- Replies the bot has posted, by a key naming what was said and where (e.g. "reply:<tip>:paid").
-- Job dedupe keys only cover jobs still queued; this table remembers replies after their jobs
-- are gone, so a re-run of the step that queued one never posts it twice.
CREATE TABLE sent_replies (
    key     text PRIMARY KEY,
    sent_at timestamptz NOT NULL DEFAULT now()
);
