-- dbm:dialect sqlite
-- dbm:transaction auto
-- dbm:up
ALTER TABLE expense ADD COLUMN category TEXT NOT NULL DEFAULT 'geral';

-- dbm:down
SELECT 1;
