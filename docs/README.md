# Документация Osy Kernel

| Документ | О чём |
|---|---|
| [architecture.md](architecture.md) | Идея, топология, модули, хранилище, API, протокол событий, контракт агента, конфиг |
| [request-flow.md](request-flow.md) | Алгоритмы обработки запросов: оркестратор, skill, tool, прямой вызов; цена сценариев в LLM-вызовах |
| [self-healing.md](self-healing.md) | Как работает восстановление сейчас, найденные пробелы, целевая схема |
| [memory.md](memory.md) | Rules, facts, карта диалога, история; текущий алгоритм RAG |
| [improvements.md](improvements.md) | Приоритизированный список улучшений с оценкой трудоёмкости |
| [notes-design.md](notes-design.md) | Проект «notes»: рабочие заметки ИИ в пределах сессии |
| [rag-facts.md](rag-facts.md) | Проект переделки поиска по фактам и статус |
| [cistern-search.md](cistern-search.md) | Поиск и индексация в `cistern`: косинус, гибридный поиск, обслуживание индекса, калибровка |

Начать лучше с `architecture.md`, затем `request-flow.md` и `self-healing.md`.
Документы описывают код на момент анализа (без внешних крейтов `anylm`, `cistern`, `pearce`, `rigging`, `atoman`):
места, зависящие от их поведения, помечены словом «проверить».
