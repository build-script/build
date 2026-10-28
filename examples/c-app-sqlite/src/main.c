#include <sqlite3.h>
#include <stdio.h>

static int print_row(void *unused, int columns, char **values, char **names) {
    (void) unused;

    for (int i = 0; i < columns; i++) {
        printf("%s=%s%s", names[i], values[i] ? values[i] : "NULL", i + 1 < columns ? ", " : "\n");
    }

    return 0;
}

int main(void) {
    sqlite3 *db;
    char *error = NULL;

    if (sqlite3_open(":memory:", &db) != SQLITE_OK) {
        fprintf(stderr, "%s\n", sqlite3_errmsg(db));
        return 1;
    }

    const char *sql =
        "CREATE TABLE notes (id INTEGER PRIMARY KEY, text TEXT);"
        "INSERT INTO notes (text) VALUES ('build-script'), ('builds C');"
        "SELECT id, text FROM notes;";

    if (sqlite3_exec(db, sql, print_row, NULL, &error) != SQLITE_OK) {
        fprintf(stderr, "%s\n", error);
        sqlite3_free(error);
        return 1;
    }

    printf("sqlite %s\n", sqlite3_libversion());
    sqlite3_close(db);
    return 0;
}
