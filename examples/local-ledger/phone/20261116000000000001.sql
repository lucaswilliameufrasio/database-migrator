CREATE TABLE expense (
    id INTEGER PRIMARY KEY,
    payee TEXT NOT NULL,
    amount_cents INTEGER NOT NULL CHECK (amount_cents > 0),
    spent_on TEXT NOT NULL
);

