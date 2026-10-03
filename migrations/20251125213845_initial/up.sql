CREATE TABLE enabled_chats(
    chat_id BIGINT NOT NULL,
    interval SMALLINT NULL,
    msgs_until_gen SMALLINT NOT NULL,
    CONSTRAINT chat_pk PRIMARY KEY (chat_id)
);
