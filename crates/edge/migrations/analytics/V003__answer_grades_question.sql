-- The (truncated) customer question that was graded, so the Gaps export can show
-- Claude Code the actual failing questions to write docs for — not just the
-- topic. Nullable: older rows carry none. Created unqualified: the migration
-- runner sets search_path to `edge_analytics` for this set.
ALTER TABLE answer_grades ADD COLUMN IF NOT EXISTS question TEXT;
