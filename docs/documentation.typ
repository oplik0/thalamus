#set text(lang: "pl")
#import "@local/wut-thesis:0.1.11": simple-doc 
#import "@preview/mmdr:0.2.2": mermaid

#show raw.where(lang: "mermaid"): it => mermaid(it.text, layout: (
    node_spacing: 100,
  ),)

#show: simple-doc.with(
  doc-type: "project",
  show-toc: true,
  draft: false,
  title: "Router LLMów - Thalamus",
  author: ("Jakub Bliźniuk", "Artur Sliepchenko"),
  course: "Zaawansowane Programowanie w C++",
  lang:  "pl",
  font-size:  10.8pt
)
= Wstęp
== Krótki opis

Celem projektu jest zaimplementowanie konfigurowalnego routera/proxy dla lokalnie hostowanych modeli językowych, zapewniającego konfigurowalne balansowanie obciążenia przy użyciu standardowego API OpenAI z obsługą uwierzytelniania kluczami API.

Serwis powinien obejmować sam router z prostym panelem webowym do zarządzania nim.

== Problem

Mamy $N$ ($N>=1$) serwerów z lokalnymi modelami, potencjalnie używających różnych backendów (np. ollama, vllm, llama.cpp) o różnej wydajności, które chcemy wykorzystywać do wielu aplikacji - potencjalnie także udostępniając je innym użytkownikom.

Idealnie z perspektywy użytkownika podstawowe użycie było by podobnie proste do opcji "chmurowych" - wrzucenie do aplikacji własnego URLa i klucza i nie myślenie o tym co się dzieje w tle, dostając jednocześnie najlepszą możliwą przy tych zasobach usługę.

Oznacza to kierowanie jego zapytań na serwery według kombinacji kilku kryteriów - ich wydajności, aktualnego wykorzystania, cache promptów, czy jakiś polityk (np. przechowywanie danych).

== Istniejące rozwiązania

- #link("https://github.com/BerriAI/litellm")[LiteLLM]: w teorii spełnia wszystkie wymagania i *da się* go skonfigurować tak by prawie zupełnie rozwiązywał problem! Niestety, jest to koszmarna baza kodu w Pythonie, gdzie na każdym kroku spotyka się z nietypowym designem, wielkimi plikami (plik main.py ma 6300 linii, a nie tak dawno został wydzielony z \_\_init\_\_.py w którym wciąż zostało \>1300 linii), dokumentacją wyglądającą miejscami na napisaną przez LLMa, czy po prostu bardzo wolnym działaniem. Nie wspominając o po prostu niechlujnym zarządzaniu repozytorium: znajdują się tam artefakty procesu budowania frontendu (co oznacza że po zbudowaniu jakoś zmodyfikowanego nie można po prostu wciągnąć nowych zmian bez spotkania się z konfliktami), puste pliki wyglądające na pozostałości po pracy z LLMami (obecnie np. pusty plik MCP\_SSL\_CHANGES\_SUMMARY.md), czy porozrzucane losowo testy (mimo istnienia folderu tests…). Dodatkowo koncepcyjnie był dostosowany bardziej do agregacji rozwiązań chmurowych, więc podstawowym obiektem tam jest model - utrudniając routing w sytuacji gdy ma się jeden backend który może się przełączać między wieloma modelami. To właśnie doświadczenia z LiteLLM stanowiły motywacje dla tego projektu…
- #link("https://portkey.ai/features/ai-gateway")[Portkey AI]: niestety w wersji otwartej nie zawiera jakiegokolwiek konceptu stałej konfiguracji, wymagając wysyłania JSONa z ustawieniami w kazdym żądaniu. By działać inaczej konieczne jest wykorzystanie wersji chmurowej. Ma też podobny problem co LiteLLM z używaniem modelu jako podstawowy koncept, nastawiając produkt bardziej na pracę z chmurowymi LLMami
- #link("https://konghq.com/ai-gateway")[Kong AI Gateway]: *możliwe*, że jest sensownym rozwiązaniem problemu, ale tak jak często k8s to przesada dla mniejszych organizacji, tak hostowana na k8s "cloud-native" usługa z własnym CLI realizując całą swoją funkcjonalność w formie 50 różnych pluginów które włącza się używając customowego formatu konfiguracji w YAMLu to chyba przesada dla każdego kto nie planuje zatrudnić osoby dedykowanej tylko do zarządzania tą usługą.
- Rozwiązania czysto chmurowe, od np. #link("https://developers.cloudflare.com/ai")[Cloudflare], #link("https://vercel.com/ai")[Vercel], #link("https://learn.microsoft.com/azure/ai-services/")[Microsoft], itp. są jeszcze bardziej skupione na dostępie do chmurowych LLMów i nie adresują ludzi chcących hostować własne modele.

= Opis rozwiązania
== Docelowe rozwiązanie

Usługa konfigurowalnego routera dla modeli językowych, pozwalająca skonfigurować wiele backendów z odpowiadającymi im modelami, zapewniająca konfigurowalne i modyfikowalne (przez pluginy - np. używając extism) strategie kierowania ruchu między nimi (i jeśli konieczne tłumaczenia między wspieranymi API) wystawionych przez uwierzytelnione endpointy kompatybilne z większością aplikacji (czyli API OpenAI).

Tj. aplikacja ma za zadanie wystawić endpointy takie jak /v1/chat/completions i na podstawie zawartych w nich informacji (w szczególności pola model) zaaplikować odpowiednie transformacje i skierować na odpowiedni serwer (także na /v1/chat/completions albo np. przetłumaczone na API ollamy).

Rozwiązanie powinno zapewniać portal webowy pozwalający użytkownikom na zalogowanie się (konto na platformie lub SSO) i zarządzanie swoimi kluczami API, a administratorom na zarządzanie backendami, modelami, użytkownikami i ich uprawnieniami.

Z założenia ma być to relatywnie proste w użyciu i lekkie (w przeciwieństwie do LiteLLM czy Kong AI Gateway), ale jednocześnie elastyczne i rozszerzalne (przez pluginy do modyfikacji requestów, konfigurację strategii routingu, itp.). Ma być skupione na hostowanie własnych modeli, ale nie wyklucza się możliwości dodania obsługi chmurowych LLMów jako backendów (w końcu czym róznią się serwery OpenAI od lokalnych z perspektywy API? :).

Idealnie byłoby też rozszerzone o funkcje takie jak kolejkowanie żądań (w szczególności endpoint batch do wysyłania wielu zapytań o niższym priorytecie), obserwowalność, wsparcie dla MCP (Model Context Protocol) i inne udogodnienia, ale projekt jest obecnie skupiony na podstawowej funkcjonalności.

== Używane technologie

- #link("https://www.rust-lang.org/")[Rust 2024] jako język backendu.
- #link("https://github.com/tokio-rs/axum")[Axum] i Tokio jako warstwa HTTP/asynchroniczna.
- #link("https://github.com/launchbadge/sqlx")[SQLx] i PostgreSQL jako trwały magazyn stanu aplikacji.
- #link("https://github.com/casbin/casbin-rs")[Casbin] do autoryzacji RBAC.
- #link("https://kcl-lang.io/")[KCL] jako język konfiguracji backendów, routingu i pluginów.
- #link("https://extism.org/")[Extism] i WebAssembly jako system rozszerzeń.
- PASETO, OPAQUE, Argon2 oraz HTTP Message Signatures (RFC 9421) w warstwie uwierzytelniania.
- Frontend w TypeScript/React Native for Web, oparty o Expo Router, React Query, Gluestack UI i NativeWind.#footnote[Obecnie nie ma planów na natywne aplikacje, ale React Native zapewnia sensowne abstrakcje i był preferowanym sposobem autora na używanie Reacta. W praktyce frontend webowy działa, ale ta decyzja miała koszt integracyjny opisany dalej.]

== Architektura

=== Ogólna architektura systemu

Aplikacja składa się z trzech głównych warstw językowych/technologicznych: backendu napisanego w Rust, panelu administracyjnego w TypeScript/React Native for Web oraz zewnętrznych serwerów modeli językowych. 

Backend jest głównym elementem systemu. Jego zadaniem jest przyjmowanie żądań zarówno od aplikacji klienckich (w formacie OpenAI/Anthropic), jak i od panelu webowego, uwierzytelnianie ich, autoryzacja, a następnie kierowanie żądań do odpowiedniego serwera modelu. Frontend komunikuje się z backendem wyłącznie przez REST API, przesyłając token PASETO albo klucz API w nagłówku `Authorization`. Baza PostgreSQL przechowuje trwały stan aplikacji (użytkownicy, zespoły, projekty, klucze API, tokeny, logi i zadania batch).

=== Architektura backendu (Rust)

Backend został zorganizowany według obszarów funkcjonalności. Każdy obszar (np. `auth`, `llm_proxy`, `routing`, `teams`, `batch`, `plugin`) posiada własny katalog, a w nim pliki odpowiadające za warstwy: `api.rs` (obsługa żądań HTTP), `domain.rs` (logika biznesowa), `infra.rs` (implementacje repozytoriów i usług zewnętrznych) oraz opcjonalnie `dto.rs` (obiekty transferu danych). Taki podział ułatwia testowanie jednostkowe i izolację zależności.

Poniższy diagram klas przedstawia najważniejsze abstrakcje wewnątrz backendu oraz relacje między nimi.
#figure(
```mermaid
classDiagram
  direction TB

  class ProxyService {
    -BackendClient client
    -BackendRegistry registry
    -RouterService router
    -GuardrailService guardrails
    +handle(LlmRequest) ChatResponse
    +handle_stream(LlmRequest) Stream
    +handle_embedding(EmbeddingRequest) Value
  }

  class BackendClient <<trait>> {
    +send(EndpointSnapshot, LlmRequest) ChatResponse
    +send_stream(EndpointSnapshot, LlmRequest) Stream
    +send_embedding(EndpointSnapshot, EmbeddingRequest) Value
  }

  class BackendRegistry <<trait>> {
    +acquire(EndpointId)
    +release(EndpointId)
    +snapshot() Vec~EndpointSnapshot~
  }

  class RoutingStrategy <<trait>> {
    +select(RoutingContext) Option~EndpointSnapshot~
    +name() &str
  }

  class RouterService {
    -BackendRegistry registry
    -RoutingStrategy strategy
    -PriorityQueueManager queue_manager
    +route(LlmRequest) EndpointSnapshot
    +route_or_queue(LlmRequest, Priority) EndpointSnapshot
  }

  class PriorityQueueManager {
    -queues VecDeque[]
    +enqueue(LlmRequest, Priority) Receiver
    +try_dispatch(fn) void
    +age_requests() void
  }

  class TeamRepository <<trait>> {
    +create(Team) Result
    +find_by_id(UUID) Option~Team~
  }

  class MembershipRepository <<trait>> {
    +add_member(UUID, UUID, Role) Result
    +list_by_team(UUID) Vec~Membership~
  }

  class PluginManager {
    -DashMap plugins
    +load_plugin(PluginManifest) Result
    +unload_plugin(str) Result
    +list_plugins() Vec~PluginInfo~
  }

  class BatchService {
    -BatchRepository repository
    -ProxyService proxy
    +create_job(BatchRequestBody) UUID
    +process_job(BatchJob) Result
  }

  ProxyService --> BackendClient : używa
  ProxyService --> BackendRegistry : używa
  ProxyService --> RouterService : używa
  RouterService --> RoutingStrategy : używa
  RouterService --> PriorityQueueManager : używa
  RouterService --> BackendRegistry : używa
  PluginManager ..> ProxyService : guardrails / adapters
  BatchService --> ProxyService : przetwarza batch jako Priority::Batch
```)


=== Architektura frontendu

Panel webowy jest aplikacją React Native for Web opartą na `expo-router`, co oznacza, że routing jest plikowy (struktura katalogów `src/app/` odwzorowuje ścieżki URL). Warstwa prezentacji korzysta z biblioteki komponentów Gluestack UI w połączeniu z NativeWind/Tailwind. Stan serwerowy jest zarządzany przez `@tanstack/react-query`, a uwierzytelnienie odbywa się przez kontekst `AuthContext` przechowujący token PASETO.

Główne ekrany aplikacji to: logowanie (`/login`), pierwsza konfiguracja (`/login/setup`), widok główny (`/(tabs)/index`) oraz widoki administracyjne dla kluczy API, podpisujących kluczy HTTP, użytkowników, zespołów, ustawień i polityk Casbin. Frontend nie komunikuje się bezpośrednio z bazą danych ani z zewnętrznymi backendami LLM. Wszystkie operacje przechodzą przez backend.

=== Komunikacja między warstwami

Komunikacja między językami/technologiami odbywa się przez dobrze zdefiniowane protokoły:

1. *Frontend (TypeScript) → Backend (Rust)*: HTTP/REST, JSON, nagłówek `Authorization: Bearer <token>`. Token jest wydawany po uwierzytelnieniu za pomocą OPAQUE (hasło) lub OAuth2. Część operacji administracyjnych może być też wykonywana z użyciem klucza API posiadającego odpowiedni scope.
2. *Klienci LLM → Backend (Rust)*: HTTP/REST zgodny z OpenAI (np. `POST /v1/chat/completions`), strumieniowanie przez Server-Sent Events (SSE). Uwierzytelnienie za pomocą klucza API przekazywanego w nagłówku `Authorization`.
3. *Backend → PostgreSQL*: protokół SQL przez TCP, obsługiwany przez `sqlx` z weryfiką zapytań w czasie kompilacji (`query!` / `query_as!`).
4. *Backend → Zewnętrzne backendy LLM*: HTTP/REST lub HTTP/SSE, zależnie od implementacji adaptera (`AdaptingBackendClient`).
5. *KCL → Rust*: plik konfiguracyjny `config.k` jest parsowany do struktur serde podczas startu aplikacji; watcher przeładowuje konfigurację w czasie działania.
6. *Plugin WASM → Backend Rust*: pluginy komunikują się przez ABI Extism i przekazują dane jako JSON mapowany na typy z crate'u `thalamus-plugin`.

== Schemat bazy danych

Baza danych PostgreSQL jest głównym magazynem stanu aplikacji. Poniższy diagram ER przedstawia encje oraz relacje między nimi.

```mermaid
erDiagram
    TEAMS ||--o{ TEAM_MEMBERSHIPS : has members
    USERS ||--o{ TEAM_MEMBERSHIPS : belongs to
    TEAMS ||--o{ API_KEYS : owns
    USERS ||--o{ API_KEYS : creates
    PROJECTS ||--o{ API_KEYS : scopes
    USERS ||--o{ USAGE_LOGS : performs
    TEAMS ||--o{ USAGE_LOGS : charged to
    API_KEYS ||--o{ USAGE_LOGS : used by
    PROJECTS ||--o{ USAGE_LOGS : scoped to
    USAGE_LOGS ||--o{ REQUEST_LOGS : detailed log
    USERS ||--o{ TOKEN_REVOCATIONS : revokes
    USERS ||--o{ REFRESH_TOKENS : has
    TEAMS ||--o{ REFRESH_TOKENS : belongs to
    USERS ||--o{ SIGNING_KEYS : owns
    TEAMS ||--o{ SIGNING_KEYS : owns
    OAUTH_PROVIDERS ||--o{ OAUTH_IDENTITIES : issues
    USERS ||--o{ OAUTH_IDENTITIES : linked to
    TEAMS ||--o{ BATCH_JOBS : owns
    USERS ||--o{ BATCH_JOBS : creates

    TEAMS {
        uuid id PK
        string name UK
        string description
        decimal budget_limit_usd
        int rate_limit_rpm
        int rate_limit_burst
        text[] allowed_models
        text[] allowed_backends
        text[] allowed_tags
        string logging_policy
        int log_retention_days
        uuid parent_team_id FK
        boolean is_active
        string slug UK
        timestamp created_at
        timestamp updated_at
        timestamp deleted_at
        string default_priority
    }

    USERS {
        uuid id PK
        string username UK
        string email UK
        boolean is_service_account
        boolean is_active
        bytea opaque_registration
        uuid oauth_provider_id FK
        uuid oauth_identity_id FK
        timestamp created_at
        timestamp updated_at
        timestamp last_login_at
    }

    TEAM_MEMBERSHIPS {
        uuid id PK
        uuid user_id FK
        uuid team_id FK
        string role
        timestamp created_at
        timestamp deleted_at
    }

    PROJECTS {
        uuid id PK
        uuid team_id FK
        string name
        string description
        jsonb metadata
        timestamp created_at
        timestamp updated_at
        timestamp deleted_at
    }

    API_KEYS {
        uuid id PK
        string key_id UK
        string key_hash
        string key_prefix
        uuid user_id FK
        uuid team_id FK
        uuid project_id FK
        string name
        string description
        text[] scopes
        boolean is_active
        timestamp last_used_at
        timestamp expires_at
        timestamp created_at
        timestamp revoked_at
        uuid rotated_from FK
        timestamp rotated_at
        timestamp grace_period_ends_at
        string rotation_reason
        string default_priority
    }

    USAGE_LOGS {
        uuid id PK
        uuid request_id
        uuid user_id FK
        uuid team_id FK
        uuid api_key_id FK
        uuid project_id FK
        string model
        string backend
        string endpoint
        int prompt_tokens
        int completion_tokens
        int total_tokens
        decimal cost_usd
        int latency_ms
        int queue_time_ms
        int status_code
        text error_message
        timestamp created_at
    }

    REQUEST_LOGS {
        uuid id PK
        uuid request_id
        uuid usage_log_id FK
        jsonb request_body
        jsonb response_body
        jsonb request_headers
        jsonb response_headers
        inet ip_address
        text user_agent
        timestamp created_at
    }

    TOKEN_REVOCATIONS {
        uuid token_jti PK
        uuid user_id FK
        timestamp revoked_at
        timestamp expires_at
        string reason
        uuid revoked_by FK
    }

    REFRESH_TOKENS {
        uuid id PK
        uuid user_id FK
        uuid team_id FK
        string token_hash UK
        uuid family
        uuid parent_token_jti
        text[] scopes
        text[] roles
        boolean is_active
        timestamp revoked_at
        string revoked_reason
        timestamp expires_at
        timestamp created_at
        timestamp last_used_at
    }

    SIGNING_KEYS {
        uuid id PK
        string key_id UK
        uuid user_id FK
        uuid team_id FK
        text public_key
        string algorithm
        string key_fingerprint
        string name
        string description
        text[] scopes
        boolean is_active
        timestamp revoked_at
        string revoked_reason
        timestamp expires_at
        timestamp last_used_at
        int use_count
        timestamp created_at
    }

    OAUTH_PROVIDERS {
        uuid id PK
        string name UK
        string provider_type
        text client_id_encrypted
        text client_secret_encrypted
        jsonb config_json
        boolean is_active
        timestamp created_at
        timestamp updated_at
    }

    OAUTH_IDENTITIES {
        uuid id PK
        uuid user_id FK
        uuid provider_id FK
        string provider_user_id
        string email
        string username
        text access_token_encrypted
        text refresh_token_encrypted
        timestamp token_expires_at
        timestamp created_at
        timestamp updated_at
    }

    CASBIN_RULE {
        int id PK
        string ptype
        string v0
        string v1
        string v2
        string v3
        string v4
        string v5
    }

    BATCH_JOBS {
        uuid id PK
        uuid team_id FK
        uuid user_id FK
        jsonb request_body
        string status
        jsonb response_body
        text error_message
        int request_count
        int completed_count
        int failed_count
        timestamp created_at
        timestamp started_at
        timestamp completed_at
    }
```#footnote[Przepraszam za jakość diagramu, ale dopiero eksportując ten dokument zauważyłem, że paczka do Typst mmdr zepsuła trochę layout w porównaniu do oryginalnego mermaid. Mam nadzieję, że jest wystarczająco czytelny.]

= Aspekty implementacji

== Zakres wykonanych prac

Rdzeń systemu działa: Thalamus przyjmuje żądania w formacie kompatybilnym z API OpenAI (`/v1/chat/completions`, `/v1/embeddings`, eksperymentalnie też `/v1/responses`) oraz podstawowy wariant Anthropic Messages (`/v1/messages`). Odpowiedzi mogą być zwykłym JSON-em albo strumieniem SSE. Po stronie wyjściowej proxy umie rozmawiać z backendami typu Ollama, vLLM/llama.cpp i ogólnymi serwerami kompatybilnymi z tym samym API OpenAI.

Routing nie jest już tylko wyborem "pierwszego pasującego URL-a". Backendy są ładowane z konfiguracji KCL do rejestru w pamięci, health-checki aktualizują ich stan, a strategie routingu mogą brać pod uwagę wagę, obciążenie, liczbę aktywnych połączeń i informację o załadowanych modelach. Jest też kontrola capacity endpointu. Jeśli wszystkie pasujące endpointy są zajęte, żądanie trafia do kolejki priorytetowej zamiast od razu kończyć się błędem. Kolejka ma timeouty, limity rozmiaru i promowanie wpisów, które czekają zbyt długo.

Duża część pracy poszła w bezpieczeństwo, prawdopodobnie zbyt duża. Klucze API mają prefiks `thl_`, są hashowane Argon2 i można je rotować. Logowanie hasłem używa OPAQUE, więc serwer nigdy nie dostaje hasła w postaci jawnej. Są też tokeny PASETO#footnote[#link("https://gist.github.com/samsch/0d1f3d3b4745d778f78b230cf6061452")[JWT to bardzo zły standard niestety]], lista unieważnień, oraz OAuth2 dla GitHuba/GitHub Enterprise/OIDC. Uprawnienia są spięte przez Casbin.

Powstał też model organizacyjny: użytkownicy, zespoły, członkostwa, projekty i hierarchia zespołów. API pozwala tworzyć i edytować te obiekty, a sprawdzanie uprawnień uwzględnia członkostwo w zespole nadrzędnym. Role są obecnie proste (`admin`, `member`, `readonly`), ale wystarczają do obecnego zakresu.

Panel webowy obsługuje pierwszą konfigurację systemu, logowanie, OAuth, klucze API, użytkowników, zespoły, ustawienia i modyfikację polityk Casbin. Jest temu daleko do gotowego produktu, ale nie jest też atrapą z dwoma przyciskami.

Konfiguracja jest pisana w KCL i może być przeładowywana bez restartu procesu. Repozytorium ma migracje SQLx, środowisko deweloperskie z PostgreSQL, Dockerfile, docker-compose oraz testy integracyjne oparte między innymi o `#[sqlx::test]` i WireMock. Obserwowalność jest na razie głównie w `tracing`; pola konfiguracyjne dla metryk i OTLP istnieją, ale pełny endpoint Prometheus nie jest jeszcze gotowy.

System pluginów działa w trzech miejscach ścieżki żądania. Plugin routingu może wybrać endpoint zamiast wbudowanej strategii. Plugin adaptera może budować żądanie HTTP i parsować odpowiedź dla backendu. Plugin guardrail może obejrzeć request albo response i go zablokować. Przykłady są w `examples/plugins/`: `routing-echo`, `adapter-echo` i `guardrail-blocklist`.

Doszedł również batch processing. `POST /v1/batch/chat/completions` zapisuje zestaw zapytań w tabeli `batch_jobs`, a `GET /v1/batch/chat/completions/{id}` zwraca status i wynik. Worker w tle bierze zadania `pending` i wykonuje je przez ten sam `ProxyService`, ale z priorytetem `Batch`, więc batch nie powinien wypychać interaktywnych żądań z kolejki.

== Rzeczy niewykonane i uzasadnienie

Nie wszystko z pierwotnej listy planów zostało dowiezione do końca. Największy brak to MCP. Nie powstało proxy Model Context Protocol, bo to nie jest tylko kolejny endpoint HTTP. MCP wymaga osobnego modelu sesji, obsługi JSON-RPC, strumieniowania komunikatów i sensownego powiązania z uprawnieniami.

Pluginy też nie pokrywają jeszcze całej zaprojektowanej powierzchni. Routing, adaptery i guardraile są podłączone, bo wpływają bezpośrednio na obsługę requestu, ale to obecnie wszystko. Brakuje w szczególności MCP, jak wspomniano wyżej, ale warto by też było gdyby część obsługi np. uprawnień dało się tak realizować (np. chętnie bym wspierał użycie pluginu do synchronizacji uprawnień z usługą typu Keycloak).

Kolejka i batch działają, ale są obecnie napisane pod jedną instancję aplikacji. Rozwinięcie tego będzie wymagało najpewniej dokończenia integracji Redisa/Valkey.

Panel webowy wymaga dalszego dopracowania. Ma podstawowe komponenty i obsługuje główne funkcje, ale jednak nawet w porównaniu do również dość prymitywnego UI LiteLLM dużo mu brakuje. React Native for Web pomógł zachować jeden model komponentów, ale stylowanie i zachowanie na webie kosztowały więcej czasu niż zakładano.

Nie ma jeszcze cache promptów. W konfiguracji istnieje Redis i `CacheConfig`, ale samo cache'owanie odpowiedzi LLM nie zostało jeszcze zaimplementowane i samo dodanie tej funkcji jest obecnie niepewne: po przemyśleniu nie jesteśmy pewni, czy w ogóle dla naszych zastosowań ma sens (głównym zastosowaniem cache są konwersacje z LLMami przez dużą ilość ludzi, pozwalając zminimalizować użycie backendu przy podobnych pytaniach. Nasze zastosowania są głównie narzędziowe i tam jednak jest obecnie oczekiwanie, że LLM rzeczywiście przetworzy żądanie).

Brakuje też kompletnej dokumentacji API/OpenAPI.

Obserwowalność jest podstawowa. `tracing` działa i pomaga debugować przepływ żądań, ale nie ma jeszcze pełnych metryk Prometheus dla dystrybucji ruchu, błędów, zdrowia backendów i użycia pluginów.

== Nieprzewidziane przeszkody i zmiany względem planu

Największym problemem okazała się warstwa uwierzytelniania, choć miało to miejsce z winy jednego z autorów#footnote[Jakub Bliźniuka, przyznaję się do wszystkich przekomplikowań poniewż odpowiadałem za architekturę :)], który nalegał na nieprzechowywanie haseł - okazuje się, że OPAQUE między JS a Rustem jednak nie jest aż tak trywialne jak fakt, że paczka `@serenity-kit/opaque` bazuje na Rustowej `opaque-ke` by wskazywał. Na szczęście trochę czytania kodu źródłowego paczek pozwoliło doprowadzić logowanie nazwą/hasłem do działania. Ironicznie wymagało to więcej pracy przez wcześniejszą decyzję o implementacji OAuth w pierwszej kolejności, jako głównie używaną metodę w praktyce (choć trudną do testowania)

Frontend też zajął więcej czasu niż powinien. React Native for Web był wygodny koncepcyjnie, ale web developer ma swoje przyzwyczajenia, szczególnie przy formularzach.

Dość przekomplikowana jest też  konfiguracja - KCL daje ciekawe możliwości, będąc w zasadzie językiem programowania we własnym zakresie, ale szczególnie małe problemy z wersjami zależności i problemy z istniejącym oficjalnym watcherem spowodowały dużo bólu przy każdej próbie aktualizacji. O ile kcl jest napisany w Ruscie to niestety używanie go jako biblioteka w tak zaawansowanym zakresie jest dość słabo wspierane obecnie.

Sporym problemem było też planowanie pracy - na przyszłość warto będzie spędzić więcej czasu najpierw organizując issues i plany oraz wiążąc z nimi pracę...

== Dlaczego projekt zasługuje na wysoką ocenę

Projekt wydaje nam się wyróżniać w szczególności praktycznością: jest stworzony by rozwiązać problemy z instniejącymi rozwiązaniami w naszej pracy i obecnie nawet testowo jest tam używany (choć na razie głównym proxy wciąż jest LiteLLM w wyniku tego, że inne obowiązki nie dały czasu na dokończenie niektórych funkcji które są aktywnie używane). Ma więc szanse być jednym z relatywnie niewielu naszych projektów ze studiów które nie tylko rozwijają się jakoś dalej po zakończeniu przedmiotu, ale nawet mają użytkowników (prawdopodobnie $~40$ osób).

Dodatkowo jest to dość skomplikowana aplikacja nawet w obecnym stanie, obejmująca poza standardowym CRUD obsługę strumieniowania, kilka metod uwierzytelniania, system pluginów, system konfiguracji oraz UI webowe.