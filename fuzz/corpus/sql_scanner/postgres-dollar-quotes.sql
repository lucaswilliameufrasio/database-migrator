DO $tag$ BEGIN
    PERFORM 'semicolon; and -- dbm:down';
    /* nested /* block */ comment; */
END; $tag$;
SELECT 2;
