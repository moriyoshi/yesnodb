package dev.yesnodb.search;

final class JsonStrings {
  private JsonStrings() {}

  static String quote(String value) {
    StringBuilder json = new StringBuilder(value.length() + 2).append('"');
    for (int index = 0; index < value.length(); index++) {
      char character = value.charAt(index);
      switch (character) {
        case '"' -> json.append("\\\"");
        case '\\' -> json.append("\\\\");
        case '\b' -> json.append("\\b");
        case '\f' -> json.append("\\f");
        case '\n' -> json.append("\\n");
        case '\r' -> json.append("\\r");
        case '\t' -> json.append("\\t");
        default -> {
          if (character < 0x20) {
            json.append(String.format("\\u%04x", (int) character));
          } else if (Character.isHighSurrogate(character)) {
            if (index + 1 >= value.length() || !Character.isLowSurrogate(value.charAt(index + 1))) {
              throw new IllegalArgumentException("JSON string contains an unpaired high surrogate");
            }
            json.append(character).append(value.charAt(++index));
          } else if (Character.isLowSurrogate(character)) {
            throw new IllegalArgumentException("JSON string contains an unpaired low surrogate");
          } else {
            json.append(character);
          }
        }
      }
    }
    return json.append('"').toString();
  }
}
