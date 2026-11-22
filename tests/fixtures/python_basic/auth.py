def validate_token(token):
    return check_signature(token)


def check_signature(token):
    return len(token) > 8
