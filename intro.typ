#set text(lang: "pl")
#import "@local/wut-thesis:0.1.11": simple-doc

#show: simple-doc.with(
  doc-type: "notes",
  show-toc: false,
  draft: false,
  title: "Router LLMów - alternatywa dla LiteLLM",
  author: "Jakub Bliźniuk",
  course: "Zaawansowane Programowanie w C++",
  lang:  "pl",
  font-size:  10.8pt
)

== Krótki opis

Celem projektu jest zaimplementowanie konfigurowalnego routera/proxy dla lokalnie hostowanych modeli językowych, zapewniającego konfigurowalne balansowanie obciążenia przy użyciu standardowego API OpenAI z obsługą uwierzytelniania kluczami API.

Serwis powinien obejmować sam router z prostym panelem webowym do zarządzania nim.

== Problem

Mamy $N$ ($N>=1$) serwerów z lokalnymi modelami, potencjalnie używających różnych backendów (np. ollama, vllm, llama.cpp) o różnej wydajności, które chcemy wykorzystywać do wielu aplikacji - potencjalnie także udostępniając je innym użytkownikom.

Idealnie z perspektywy użytkownika podstawowe użycie było by podobnie proste do opcji "chmurowych" - wrzucenie do aplikacji własnego URLa i klucza i nie myślenie o tym co się dzieje w tle, dostając jednocześnie najlepszą możliwą przy tych zasobach usługę.

Oznacza to kierowanie jego zapytań na serwery według kombinacji kilku kryteriów - ich wydajności, aktualnego wykorzystania, cache promptów, czy jakiś polityk (np. przechowywanie danych).

== Istniejące rozwiązania

- #link("https://github.com/BerriAI/litellm")[LiteLLM]: w teorii spełnia wszystkie wymagania i *da się* go skonfigurować tak by prawie zupełnie rozwiązywał problem! Niestety, jest to koszmarna baza kodu w Pythonie, gdzie na każdym kroku spotyka się z nietypowym designem, wielkimi plikami (plik main.py ma 6300 linii, a nie tak dawno został wydzielony z \_\_init\_\_.py w którym wciąż zostało \>1300 linii), dokumentacją wyglądającą miejscami na napisaną przez LLMa, czy po prostu bardzo wolnym działaniem. Nie wspominając o po prostu niechlujnym zarządzaniem repozytorium: znajdują się tam artefakty procesu budowania frontendu (co oznacza że po zbudowaniu jakoś zmodyfikowanego nie można po prostu wciągnąć nowych zmian bez spotkania się z konfliktami), puste pliki wyglądające na pozostałości po pracy z LLMami (obecnie np. pusty plik MCP\_SSL\_CHANGES\_SUMMARY.md), czy porozrzucane losowo testy (mimo istnienia folderu tests…). Dodatkowo koncepcyjnie był dostosowany bardziej do agregacji rozwiązań chmurowych, więc podstawowym obiektem tam jest model - utrudniając routing w sytuacji gdy ma się jeden backend który może się przełączać między wieloma modelami. To właśnie doświadczenia z LiteLLM stanowiły motywacje dla tego projektu…
- #link("https://portkey.ai/features/ai-gateway")[Portkey AI]: niestety w wersji otwartej nie zawiera jakiegokolwiek konceptu stałej konfiguracji, wymagając wysyłania JSONa z ustawieniami w kazdym żądaniu. By działać inaczej konieczne jest wykorzystanie wersji chmurowej. Ma też podobny problem co LiteLLM z używaniem modelu jako podstawowy koncept, nastawiając produkt bardziej na pracę z chmurowymi LLMami
- #link("https://konghq.com/ai-gateway")[Kong AI Gateway]: *możliwe*, że jest sensownym rozwiązaniem problemu, ale tak jak często k8s to przesada dla mniejszych organizacji, tak hostowana na k8s "cloud-native" usługa z własnym CLI realizując całą swoją funkcjonalność w formie 50 różnych pluginów które włącza się używając customowego formatu konfiguracji w YAMLu to chyba przesada dla każdego kto nie planuje zatrudnić osoby dedykowanej tylko do zarządzania tą usługą.
- Rozwiązania czysto chmurowe, od np. #link("https://developers.cloudflare.com/ai")[Cloudflare], #link("https://vercel.com/ai")[Vercel], #link("https://learn.microsoft.com/azure/ai-services/")[Microsoft], itp. są jeszcze bardziej skupione na dostępie do chmurowych LLMów i nie adresują ludzi chcących hostować własne modele.

== Docelowe rozwiązanie

Usługa konfigurowalnego routera dla modeli językowych, pozwalająca skonfigurować wiele backendów z odpowiadającymi im modelami, zapewniająca konfigurowalne i modyfikowalne (przez pluginy - np. używając extism) strategie kierowania ruchu między nimi (i jeśli konieczne tłumaczenia między wspieranymi API) wystawionych przez uwierzytelnione endpointy kompatybilne z większością aplikacji (czyli API OpenAI).

Tj. aplikacja ma za zadanie wystawić endpointy takie jak /v1/chat/generate i na podstawie zawartych w nich informacji (w szczególnosci pola model) zaaplikować odpowiednie transformacje i skierować na odpowiedni serwer (także na /v1/chat/generate albo np. przetłumaczone na API ollamy).

Rozwiązanie powinno zapewniać portal webowy pozwalający użytkownikom na zalogowanie się (konto na platformie lub SSO) i zarządzanie swoimi kluczami API, a administratorom na zarządzanie backendami, modelami, użytkownikami i ich uprawnieniami.

Z założenia ma być to relatywnie proste w użyciu i lekkie (w przeciwieństwie do LiteLLM czy Kong AI Gateway), ale jednocześnie elastyczne i rozszerzalne (przez pluginy do modyfikacji requestów, konfigurację strategii routingu, itp.). Ma być skupione na hostowanie własnych modeli, ale nie wyklucza się możliwości dodania obsługi chmurowych LLMów jako backendów (w końcu czym róznią się serwery OpenAI od lokalnych z perspektywy API? :).

Idealnie byłoby też rozszerzone o funkcje takie jak kolejkowanie żądań (w szczególności endpoint batch do wysyłania wielu zapytań o niższym priorytecie), obserwowalność, wsparcie dla MCP (Model Context Protocol) i inne udogodnienia, ale projekt jest obecnie skupiony na podstawowej funkcjonalności.

== Używane technologie

- #link("https://www.rust-lang.org/")[Rust]
- #link("https://github.com/tokio-rs/axum")[Axum] (framework webowy do stworzenia API)
- #link("https://github.com/launchbadge/sqlx")[SQLx] (obsługa bazy danych)
- #link("https://kcl-lang.io/")[KCL]
- #link("https://extism.org/")[Extism] (system pluginów)
- Frontend w React Native for Web#footnote[Obecnie nie ma planów na natywne aplikacje, ale React Native zapewnia dość sebnsowne abstrakcje i jest preferowanym przez jednego z autorów sposobem na używanie Reacta]

== Obecny stan

Projekt ma zaimplementowaną podstawową funkcjonalność. Obecnie można skonfigurować backendy i modele, a router będzie kierował zapytania do nich na podstawie pola `model` według jednej z kilku konfigurowalnych strategii. Istnieje podstawowy webowy, ale poza zarządzaniem kluczami API większość konfigurowacji odbywa się obecnie przez (automatycznie przeładowywany) plik KCL. System pluginów nie jest jeszcze zaimplementowany.

== Planowane prace

Główne elementy do implementacji obecnie to system pluginów, dokończenie implementacji zespołów, usprawnienia routera oraz proxy MCP.
