-- The collapsed status comment is re-rendered from these lines on every update, so appending a
-- line never needs to read the comment back from GitHub.
ALTER TABLE bot_comments ADD COLUMN lines text[] NOT NULL DEFAULT '{}';
