import auth
import db


def main():
    if auth.validate_token("abcdefghi"):
        db.save_session(1)
