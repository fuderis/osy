# Notes: рабочие заметки ИИ в пределах сессии

## Зачем

Контекст оркестратора = последняя пара + карта диалога (по 160 символов на сторону).
Всё, что было уточнено на третьем ходу («формат вывода — таблицы», «работаем в ветке `feat/x`», «выбран вариант B»),
к десятому ходу из контекста исчезает. `read_history` помогает, но модель должна сама догадаться, что нужно искать.

**Notes — короткий блэкнот модели для самой себя**: она сама записывает важные детали и сама их удаляет.
Заметки всегда на виду, занимают десятки токенов и живут ровно столько, сколько сессия.

## Место среди механизмов памяти

| | Rules | Facts | **Notes** |
|---|---|---|---|
| Автор | пользователь | модель / пользователь | **модель** |
| Область | user или session | user (между сессиями) | **session** |
| Попадание в контекст | всегда | по запросу (RAG) | **всегда** |
| Размер | неограничен | неограничен | **≤ 12 строк × 240 символов** |
| Назначение | «как себя вести» | «что знаю о пользователе» | **«что я решил и уточнил в этой работе»** |
| Жизнь | пока не удалят | пока не удалят | **до `clear` / удаления сессии** |

Правило выбора для модели: *о пользователе навсегда* → fact; *для этой беседы* → note; *как себя вести* → rule (создаёт только пользователь).

## Модель данных

Один ключ в таблице сессии (рядом с `Metadata` и `Message(i)`): одно чтение за итерацию, без отдельной таблицы.

```rust
// crates/share/src/note.rs
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionNote {
    pub id: u16,                       // короткий ID (в промпте дёшево)
    pub text: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionNotes {
    pub next_id: u16,                  // счётчик: ID не переиспользуются
    pub items: Vec<SessionNote>,
}
```

```rust
// user/session.rs
pub enum Key { Metadata, Message(usize), Notes }   // + Notes
const NOTES_MAX: usize = 12;
const NOTE_MAX_CHARS: usize = 240;
```

## Tool: один, пакетный

Один tool вместо трёх экономит токены на схемах и сводит работу с заметками к одному вызову.

```rust
// skills/notes.rs
pub fn tools_list() -> Vec<Tool> {
    vec![Tool::typed::<NotesAction>(
        "notes",
        "Private scratchpad for THIS session. Save short details you will need in later turns \
         and that are not in the last exchange: clarified requirements, decisions, paths, IDs, \
         partial results. One line per note. Drop outdated notes. \
         Long-term facts about the user go to remember_fact instead.",
    )]
}

#[derive(Deserialize, Schema)]
pub struct NotesAction {
    /// New notes, one short line each.
    #[serde(default)]
    pub add: Vec<String>,
    /// IDs of notes to delete.
    #[serde(default)]
    pub drop: Vec<u16>,
}
```

> Разрешить несколько вызовов в одном ответе (вызовы из одного стрима обрабатываются последовательно).

Логика на стороне сессии (эскиз, не компилировался):

```rust
impl Session {
    pub async fn load_notes(&self) -> Result<SessionNotes> {
        let table = self.kv_db.open_table(&str!(self.id)).await?;
        Ok(table.read::<_, SessionNotes>(Key::Notes).await?.unwrap_or_default())
    }

    pub async fn apply_notes(&self, add: Vec<String>, drop: Vec<u16>) -> Result<String> {
        let mut n = self.load_notes().await?;
        let before = n.items.len();
        n.items.retain(|x| !drop.contains(&x.id));
        let dropped = before - n.items.len();

        let (mut added, mut full) = (0, false);
        for text in add {
            // не utils::truncate_inline: strip_markdown съест `_`, `*` и обратные кавычки в путях
            let text: String = text.trim().chars().take(NOTE_MAX_CHARS).collect();
            if text.is_empty() || n.items.iter().any(|x| x.text == text) { continue; }
            if n.items.len() >= NOTES_MAX { full = true; break; }
            n.next_id += 1;
            n.items.push(SessionNote { id: n.next_id, text, created_at: Utc::now() });
            added += 1;
        }

        let table = self.kv_db.open_table(&str!(self.id)).await?;
        table.write(Key::Notes, n.clone()).await?;
        table.flush().await?;

        Ok(if full {
            format!("Notes full ({NOTES_MAX}): added {added}. Drop outdated ones, then add the rest.")
        } else {
            format!("Notes: +{added} -{dropped} ({}/{NOTES_MAX})", n.items.len())
        })
    }
}
```

Переполнение — **ответ модели, а не ошибка**: модель сама решает, что устарело. Это тот же принцип «ошибка как наблюдение» из [self-healing.md](self-healing.md).

## Подключение к обработке запроса

1. **`Manager::gen_basic_tools`**: добавить `notes::tools_list()` в `tools` оркестратора. В `skill_tools` не добавлять:
   skills изолированы, нужную деталь оркестратор передаёт в `query`.
2. **`handle_query`, шаг 1**: `session.load_notes()` → блок в `system_body` (после rules, перед facts):

   ```
   YOUR SESSION NOTES (written by you; keep short, drop outdated):
     [1] output format: tables only
     [4] repo ~/dev/osy, branch feat/notes
   ```

   Пустой список — блок не добавляется (ноль токенов).
3. **Стрим планирования**: новая ветка `"notes"` рядом с `remember_fact`; результат — в `memory_results` как «Memory Operation Result».
4. **Тихие операции.** Запись заметки не должна запускать control-итерацию (лишний LLM-вызов с полным контекстом).
   Заметки и `remember_fact` помечаются как *silent*: если в шаге были только silent-операции и модель уже выдала текст,
   запрос завершается без рекурсии. Если текста нет, как сейчас, идёт control-итерация.
   Эта правка дешёвая и сама по себе экономит вызов на каждом «запомни».
5. **Жизненный цикл.**
   - `Session::clear` очищает `Key::Notes`.
   - `Session::duplicate` копирует заметки.
   - `finish` для `uid = 0` стирает вместе с сессией.
6. **API и CLI.** По образцу rules: `/sessions/{sid}/notes/{list,remove,clear}`; в чате `/notes list|remove <id>|clear`.
   Пользователь видит и может править заметки: прозрачность важна, потому что это скрытый от диалога канал.
   Расширение: `/notes promote <id>` превращает заметку в факт.

## Промпт

В `ASSISTANT_PROMPT` добавить (~60 токенов, один раз):

```
Notes: a private scratchpad for this session, shown below on every turn.
Add a note when the user clarifies a requirement or you make a decision that later turns depend on.
One line per note. Remove outdated notes. Do not duplicate facts or rules.
```

## Бюджет токенов

12 × ~25 токенов ≈ 300 токенов в худшем случае, типично 3–5 заметок ≈ 100 токенов. Блок пуст, пока модель ничего не записала.
Для сравнения: расширение карты диалога с 160 до 400 символов на сторону стоит больше и менее точно.

## Что решено не делать

| Вариант | Почему нет |
|---|---|
| Заметки в `Messages` как обычные сообщения | Вернётся проблема контекстного шума и они уйдут за границу «последней пары» |
| Заметки в таблице фактов (RAG) | Нужны всегда, а не по сходству; лишние эмбеддинги |
| Свободный markdown-файл блокнота | Неограниченный рост, нет лимитов и ID для точечного удаления |
| Доступ skills к заметкам | Нарушает изоляцию; skill получает только `query` |
| Автоматическая суммаризация вместо заметок | Лишний LLM-вызов, потеря контроля над тем, что важно |

## Критерии приёмки

1. Уточнение на ходу 2 («ответы только в таблицах») соблюдается на ходу 12, когда в карте диалога оно уже обрезано.
2. Запись заметки вместе с ответом: **1** LLM-вызов на ход.
3. 13-я заметка возвращает модели «Notes full», а не ошибку; после `drop` добавление проходит.
4. `/notes clear` и `/clear` очищают блок; `clone` сессии сохраняет заметки.
5. Skills не получают заметки (проверяется дампом контекста skill).

## Открытые вопросы

- Нужен ли TTL (автоудаление заметок старше N ходов)? Начать без него: лимит 12 уже защищает от разрастания.
- Нужен ли `pinned` (не вытеснять)? Пока нет: вытеснения нет, есть только явный `drop`.
