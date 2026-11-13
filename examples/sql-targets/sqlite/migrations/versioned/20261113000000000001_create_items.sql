-- dbm:dialect sqlite
-- dbm:transaction auto
-- dbm:up
CREATE TABLE items (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL
);

-- dbm:down
DROP TABLE items;
