def seed_users(db):
    db.execute("INSERT INTO users (name) VALUES ('ada')")
    db.execute("INSERT INTO users (name) VALUES ('grace')")
