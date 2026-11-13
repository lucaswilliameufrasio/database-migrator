-- dbm:dialect postgres
-- dbm:transaction auto
-- dbm:up
CREATE TABLE items (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name TEXT NOT NULL
);

-- dbm:down
DROP TABLE items;
