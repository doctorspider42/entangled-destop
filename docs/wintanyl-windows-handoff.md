# Windows z Wintanylem — przekazanie sesji z 25 listopada 2026

Punkt odniesienia to Entangled `2004fdd` i Wintanyl `2b9e1ec` z main. Pomiary poniżej wykonano wcześniej na F239 lub na małych programach testowych. Pełny dziennik wariantów sterownika, śladów ETW i testów regresji zawiera [ADR-0010](adr/0010-wddm-desktop-on-the-gpu.md).

## Priorytet po próbie użytkownika — menu pustego pulpitu

Użytkownik ocenia animacje jako przeważnie płynne. Najbardziej przeszkadza mu prawy klik na pustym pulpicie Wintanyla, po którym widzi samo rozmycie. Poniższe pomiary animacji są tłem do dalszych prac.

Na tym samym buildzie powłoki 2b9e1ec scena `--render-png --view menu` na hoście i w VM w domyślnym trybie GPU dała identyczny plik PNG (SHA-256 `3684A63B0A591DAAED548C92D2752FACAEA659CD44FB94DE5B7932B84B283BD9`). Ta scena składa menu poza ekranem. W żywej powłoce `open_overlay` pokazuje HWND, a potem przekazuje treść do wątku UI. Osobny komunikat aktualizuje rozmycie.

W jednej próbie na działającej VM prawy klik na odsłoniętej tapecie w (1650, 430), przy 1920×1080, pokazał pozycje „Szukaj…”, „Nowy terminal” i „Preferencje”. Były widoczne zarówno w przechwyceniu ekranu Windows gościa po ok. 2 s, jak i w późniejszym zrzucie Entangled. Pliki dowodowe na hoście to `F:/VMs/Entangled/d3d12-lab/share/live-2b9e1ec/live-menu.png` i `F:/VMs/Entangled/d3d12-lab/live-2004fdd/live-menu.png`. Próba obejmowała odsłonięty fragment pulpitu przy otwartych oknach i pojedyncze otwarcie. Czas do pierwszej czytelnej klatki pozostaje niezmierzony.

Przy kolejnym wystąpieniu zapisać pozycję kliknięcia, stan okien i czas utrzymywania się rozmycia. Porównać pierwsze klatki po kliknięciu po stronie gościa i Entangled, a następnie powtórzyć ten sam scenariusz na natywnym Windows w izolowanej sesji. Treść obecna w gościu i brakująca na wyjściu VM zawęża problem do prezentacji. Gdy znika już w gościu, sprawdzić kolejność pokazania overlay, pierwszego Present i aktualizacji rozmycia oraz porównać z renderowaniem programowym. Pomiary F238/F239 dotyczyły menu pliku w Eksploratorze.

## Co sprawdzono na RTX 2070

- DWM zaimportował flip D3D12 przez Venus. Po potwierdzeniu pierwszego odczytu producent mógł pominąć kopię CPU. W teście DirectComposition 320×240 mediana draw + Present + DwmFlush spadła z 16,36 do 13,97 ms (42 pomiary na wariant).
- Dla tekstur D3D10/11 sprawdzono zgodność wszystkich 65 536 pikseli między procesami i import tego samego bufora przez DWM. Producent nadal tworzy kopię CPU dla starszych odbiorców.
- W F239 mediana / maksimum od wejścia do zmiany pikseli wyniosły 71 / 104 ms dla minimalizacji, 146 / 304 ms dla przywracania i 47 / 133 ms dla kostki. W najwolniejszym przywróceniu 227 ms przypadło po Commit, z czego oczekiwania Venus w DWM pokryły 7,6 ms. W starszym ETW w pięciu z sześciu przywróceń DWM spędzał 31–44 ms na CPU, głównie przy uploadzie współdzielonej tekstury. Pierwszy Commit kostki trwał 48,6 ms, kolejne 0,04–0,15 ms.
- Próbowano zmian oczekiwania na ring Venus, odroczonych oczekiwań hosta, równoległych fence i MSI-X. Rozbicie czasu dla tych wariantów jest w ADR. Pełne porównanie A/B animacji przerwał Defender gościa podczas uruchamiania poprzedniej binarki.
- Test podmiany shadera kończył się naruszeniem dostępu 0xC0000005 na bazowym UMD 20.43.25.803 w pierwszej iteracji. W wariancie Zinka z własną referencją do bieżącego programu, UMD 21.18.2.878, przeszedł pięć pełnych iteracji i zakończył się tym samym kodem w szóstej. Dowody z gościa to `F:/VMs/Entangled/d3d12-lab/share/zink-program-lifetime/{baseline,patched}.log`. Patch, integracja w prepare-d3d10.ps1, test i fragment ADR pozostały w indeksie roboczym Entangled.

## Co zostało

1. Zebrać stos awarii dla szóstej iteracji na poprawionym UMD i porównać go z bazowym bind_gfx_stage. Sprawdzić osobno zgłoszoną awarię uchwytu NT.
2. W tej samej VM i na tych samych binarkach Wintanyla dokończyć naprzemienne pomiary CPU/GPU od wejścia do zmiany obrazu. Zapisać wersje sterownika, ustawienia i log uruchomienia każdej próby.
3. Zebrać świeży ETW z najwolniejszego przywrócenia oraz pierwszego Commit kostki. Dotychczasowe ślady pochodzą z różnych przebiegów. Zysk dla całej powłoki oraz wydajność bliska natywnej pozostają do zmierzenia.

Profil GPU to `F:/VMs/Entangled/d3d12-lab/physical-wheel/bench.toml`, z dyskiem `d3d12-lab/lab.raw` i udziałem `d3d12-lab/share` widocznym w gościu jako Z:. Przed otwarciem dysku sprawdzić procesy Entangled. Uruchomionego Windowsa zamykać z wnętrza gościa.

## Zweryfikowany start z aktualnego main

Po zamknięciu starej VM przez Windows uruchomiono ten sam profil i dysk na natywnym silniku release z commitu `2004fdd`. Katalog uruchomienia na hoście to `F:/VMs/Entangled/d3d12-lab/live-2004fdd` (start.py, vm.pid, cmd.txt i run.log). Log hosta potwierdza NVIDIA GeForce RTX 2070 dla Venus. Na dysku gościa jest eksperymentalny sterownik labu 21.18.2.878.

Wintanyl zbudowano z `2b9e1ec` przez Cargo w profilu release z --locked. Pakiety to wt-shell, wt-guard, wt-terminal, wt-explorer, wt-shellext i wt-indexd. W gościu `Z:/live-2b9e1ec/start.cmd` kopiuje pliki do profilu użytkownika, sprawdza sześć sum SHA-256 i uruchamia guarda z --no-autostart. Zwykłe copy zwróciło błąd odczytu stronicowanego dla Eksploratora. Robocopy /J przeniosło wszystkie sześć plików, a kontrola sum przeszła. W `Z:/live-2b9e1ec/launch.log` powłoka ma Responding=True i załadowany entangled_d3d12.dll. Zrzut kontrolny pokazał pulpit „Praca” i dock. Skrót powrotu do powłoki Windows to Ctrl+Alt+Shift+End w gościu.

## Nawrót problemu i przeciąganie okien w tej samej VM

Użytkownik ponownie zobaczył samo rozmycie menu. Przy szybkim przeciąganiu Eksplorator Wintanyla wyglądał płynnie, a Notatnik się zacinał. W chwili późniejszej obserwacji oba procesy odpowiadały. HWND gościa powiązano z procesami przez MainWindowHandle. Obydwa okna mają w domyślnych regułach dekorację standard.

- 21:42:46, Notatnik (0x1048e). Galareta przejęła źródło Windows Graphics Capture po 53 ms. Następnie D3D12 CreateCommittedResource zgłosiło zawieszenie urządzenia i efekt przeszedł w ślizg zastępczy.
- 21:42:50, Eksplorator Wintanyla (0x7019c). WGC po 19 ms, potem ten sam błąd i przejście w ślizg. W kolejnym chwycie po 29 ms padło SetSwapChain. Te pojedyncze czasy dotyczą przejęcia źródła. Różnica płynności wymaga pomiaru kolejnych klatek podczas ruchu.
- Guard wcześniej zanotował dwa kolejne zakończenia procesów powłoki (0xc0000005 i 0xc0000409) i wznowił ją. W drugim procesie wątek wt-desktop zgłosił panikę w Surface::get_current_texture_view z powodem „Surface is not configured for presentation”. Brakuje jednoczesnego śladu z chwili, gdy zniknęła treść menu.
- Pierwsze zadania diagnostyczne to zapis HRESULT z GetDeviceRemovedReason w momencie błędu tworzenia zasobu i sprawdzenie obsługi nieudanej konfiguracji powierzchni przez Wintanyl. Porównać identyczny ruch obu okien z tym samym efektem i zmierzyć odstępy między widocznymi klatkami.

Dane gościa z tej sesji są w F:/VMs/Entangled/d3d12-lab/share/live-2b9e1ec (drag-observation.txt, drag-events.txt, drag-panic-detail.txt).

## Poprawki Wintanyla i długi ruch w VM

Lokalne zmiany na bazie 2b9e1ec są w repo Wintanyl. Wt-frames scala sąsiadujące aktualizacje geometrii dla tego samego okna, ogranicza ponawianie niezastosowanego przesunięcia i uwzględnia współrzędne puszczenia. Wt-effects odmierza wygaszanie według czasu klatki bez dotychczasowego limitu 50 ms. Wt-ui po konfiguracji powierzchni DX12 sprawdza obecność swapchaina, zanim spróbuje odebrać pierwszą klatkę. Brak buforów kieruje do istniejącej obsługi błędu konfiguracji.

W profilu **tej VM** ustawiono w profile/roaming/effects.toml styl galarety `glide`. Styl `sheet` pozostał domyślny w źródłach. Na pierwszym kandydacie z arkuszem powłoka zakończyła się kodem 0xc0000005 podczas serii ruchów. Zdarzenia WER z 21:39 i 21:48 dla osobnych awarii 0xc0000409 wskazują wintanyl.exe + 0x232f35. W jednym przypadku log wątku wt-desktop wskazuje nieskonfigurowaną powierzchnię. Brakuje stosu dla naruszenia dostępu i potwierdzenia jego modułu. Użytkownik nie obserwuje awarii na fizycznym Windows.

Na końcowym buildzie (SHA-256 powłoki 3771309C5314908AAE09EA54A8A0FE23A34FCCD0E5DA586D0B5DF6C62CE4C1E4, katalog gościa %LOCALAPPDATA%/Entangled-Live/drag-stable/bin) wykonano po dwa ruchy po 22 s w każdej aplikacji. Próbnik wykonywał oscylacje poziome o amplitudzie 110 px, wracał na start i puszczał przycisk. Odczyt GetWindowRect trwał jeszcze 1,3 s.

| Okno | Ostatnia zaobserwowana zmiana pozycji po puszczeniu, próby 1 / 2 | Końcowy błąd położenia | 95. percentyl błędu podczas ruchu, próby 1 / 2 |
|---|---|---|---|
| Notatnik | 14 ms / bez dalszej zmiany | 0 / 0 px | 79 / 146 px |
| Eksplorator Wintanyla | 16 / 15 ms | 0 / 0 px | 21 / 33 px |

W wyjściowym buildzie, przy krótszym ruchu przez 6,5 s, najgorszy wynik Eksploratora wyniósł 329 ms, a Notatnik pozostał w dwóch próbach 44 i 107 px od punktu końcowego. Serie mają różną długość i małą liczbę powtórzeń. Pomiar dotyczy geometrii prawdziwego okna w gościu. Ogon widocznej deformacji oraz czas do pikseli na fizycznym monitorze wymagają odrębnego pomiaru. Wspólny katalog dowodów to share/live-2b9e1ec, w nim before.csv, stable.csv, drag-crash-modules.txt, report-archives.txt i źródło próbnika w drag-stable/DragLong.cs.

Guard utrzymał ten sam PID 9128 przez cztery końcowe próby. Po nich trzy z czterech kliknięć na pustym pulpicie pokazały menu na zrzucie po 650 ms. W pierwszej próbie tekstu jeszcze nie było. Nowo uruchomiony Eksplorator z poprawką odpowiadał. Terminal zakończył się 0xc0000409, więc jego awaria nadal wymaga diagnozy. Testy bibliotek release: 16 wt-ui, 10 wt-effects i 34 wt-frames przeszły, 4 wt-frames pominięto. Zmiany pozostały lokalne.

## Maszyny po sprzątaniu

Do zadania służy jeden uruchomiony Windows z GPU: F:/VMs/Entangled/d3d12-lab/lab.raw. Za zgodą właściciela usunięto obrazy F:/VMs/wintanyl/e.raw i D:/Entangled-wintanyl-dual-monitor/d.raw. Stare profile wskazujące na nie wymagają ponownego przygotowania dysku przed użyciem. Zachowano oryginalny win7.raw, osobny ubuntu-steam.raw i niezwiązany z sesją obraz XP. Usunięto czysty worktree custom-dwm-implementation-44ecf1, gałąź została zachowana. Worktree wintanyl-virtiofs zawiera lokalną zmianę w apps/entangled/src/main.rs.

## Terminal DX12 i menu pustego pulpitu (25 listopada)

W terminalu odtworzono błąd podczas tworzenia potoku „wt-term grid” (HRESULT 0x887A0005). Wintanyl przekazywał rodzaj glifu jako Uint32, a funkcja vertex_float_format w guest/windows-gpu/d3d12/ddi-input-layout.inc w tej wersji Entangled obsługuje formaty zmiennoprzecinkowe. W wt-term-render zmieniono typ tego atrybutu na Float32 i dokonano konwersji w shaderze. Wartości 0–6 są reprezentowane dokładnie. Zmiana dotyczy Wintanyla. Obsługa wejść całkowitoliczbowych w sterowniku Entangled pozostaje otwarta.

Po zmianie uruchomiony terminal miał po 10 s odpowiadające okno i załadowany entangled_d3d12.dll. Osobno komenda --render-png w gościu poprawnie narysowała litery, cyfry i kursor. Materiał w katalogu udziału live-2b9e1ec: menu-terminal/term-result.txt, menu-cold/gpu-glyphs.png.

W powłoce wątek okna pokazuje nakładkę po zainstalowaniu elementów menu. Menu nie zamawia drugiego rozmycia pod panelem. Przy pierwszej obsłudze ukrytego okna powłoka przygotowuje glify dla pozycji menu pulpitu. Test obejmował klik prawym na pustym pulpicie (1840,290), zrzuty po ok. 120, 650, 1800 i 3500 ms, Escape, powtórzyć cztery razy. W wersji po pierwszych dwóch zmianach menu po restarcie nie było czytelne po 650 ms. Pojawiło się do 1800 ms. Po dodaniu przygotowania glifów pierwszy klik był czytelny po 130 ms. W tej serii 3/4 otwarć było widocznych na pierwszym zrzucie i 4/4 na drugim. Materiał: menu-timing-before, menu-timing-after, menu-timing-cold i logi obs.txt. Czas oznacza moment próbkowania w Windows gościa.

Działający zestaw jest w %LOCALAPPDATA%/Entangled-Live/menu-cold/bin, z profilem 2b9e1ec/profile. SHA-256 powłoki: 2CA84025D1682617D1B3AC7E0BDF8D10A8C736CAF368EB41A7487BAEC1CB3D1C. Dla kolejnych prób użyć restart-menu-cold.cmd na udziale. Skrypt działa na zainstalowanym zestawie. Osobno pozostają do diagnozy sporadyczne awarie samej powłoki (potrzebny stos z chwili awarii) i opóźnienie widocznej deformacji okien przy długim przeciąganiu.

Powtórka po osobnym restarcie tej samej powłoki (menu-timing-repeat): pierwszy klik pokazał treść na drugim zrzucie po 665 ms. Łącznie w dwóch seriach tekst był widoczny dla 4/8 kliknięć na pierwszym zrzucie (ok. 130 ms) i dla 8/8 na drugim (do 673 ms). Długotrwały nawrót wymaga dalszej obserwacji.

Na tym samym końcowym buildzie powtórzono dwa przeciągnięcia po 22 s dla każdej aplikacji. Ostatni ruch geometrii po puszczeniu w Notatniku: bez dalszego ruchu / 31 ms, w Eksploratorze: 16 / 15 ms. Błąd końcowy wyniósł 0 px we wszystkich próbach. Podczas ruchu 95. percentyl błędu położenia wyniósł odpowiednio 91 / 75 px i 103 / 17 px. Po przeciąganiu tekst menu był widoczny już na pierwszym zrzucie przy każdym z czterech kliknięć. Powłoka nadal odpowiadała z tym samym PID 5748. Dowody: cold-drag.csv, menu-after-drag, cold-final-state.txt. Testy bibliotek w profilu release: 37 zaliczonych, 1 pominięty (shader tapety DX12).
