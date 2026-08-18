# VMHost — backlog MVP

## 1. Cel MVP

MVP ma być własnym, niewielkim VMM-em uruchamianym na hoście Linux x86-64. Nie korzysta z QEMU jako procesu ani biblioteki. Kod hosta powstaje w Rust na bazie KVM i komponentów z ekosystemu rust-vmm. Interfejs graficzny w .NET/Avalonia nie jest częścią rdzenia MVP — pierwszą powierzchnią sterowania będzie CLI.

MVP powinno pozwalać użytkownikowi:

1. Pobrać oficjalny instalator stabilnego Debiana.
2. Zweryfikować podpis i sumę kontrolną pobranych plików.
3. Utworzyć pusty wirtualny dysk RAW.
4. Uruchomić Debian Installera bez klasycznego BIOS-u/UEFI.
5. Zainstalować Debiana na wirtualnym dysku.
6. Uruchomić zainstalowany system.
7. Wyświetlić pulpit w oknie przynajmniej 1920×1080.
8. Obsłużyć klawiaturę i mysz.
9. Zapewnić połączenie sieciowe potrzebne instalatorowi `netinst`.
10. Używać GPU hosta do prezentacji i skalowania obrazu bez passthrough.

Podstawowe 2D przez `virtio-gpu` jest obowiązkowe. Akcelerowane OpenGL przez VirGL/Rutabaga jest rozszerzeniem MVP i osobnym milestone'em.

## 2. Świadomie odłożone funkcje

- bootowanie dowolnego ISO przez emulowany BIOS/UEFI;
- OVMF, Secure Boot i TPM;
- Windows jako guest;
- USB i emulacja xHCI;
- audio;
- snapshoty i format QCOW2;
- suspend/resume;
- migracja VM;
- wiele monitorów;
- akcelerowany Vulkan w gueście;
- pełny panel zarządzania w Avalonia/.NET;
- obsługa dowolnego systemu operacyjnego i dowolnego kernela.

## 3. Definition of Done

Poniższy przepływ ma działać na wspieranym hoście:

```bash
vmhost fetch debian --channel stable --arch amd64 --variant gtk-netboot
vmhost disk create debian.raw --size 32G
vmhost install debian --disk debian.raw
vmhost run debian.toml
```

Rezultat:

- instalator Debiana pojawia się w oknie VM;
- instalator widzi dysk `/dev/vda`;
- instalator otrzymuje adres przez DHCP i może pobierać pakiety;
- instalacja kończy się sukcesem;
- zainstalowany Debian uruchamia się z `debian.raw`;
- Weston uruchamia się automatycznie;
- obraz działa w minimum 1920×1080;
- klawiatura i mysz działają;
- zamknięcie VM nie pozostawia procesów vCPU ani kontekstów graficznych;
- hostowy komponent nie zawiera bibliotek copyleft;
- 100 kolejnych uruchomień testowego obrazu kończy się sukcesem.

---

# 4. Skąd pobieramy Debiana

Na dzień przygotowania backlogu aktualnym stabilnym wydaniem jest **Debian 13.6.0 Trixie**. Kod nie może jednak przywiązywać się do numeru `13.6.0`; powinien korzystać z kanału `stable` albo katalogu `current` i dopiero z metadanych ustalać aktualną wersję.

## 4.1. Oficjalne strony

- Główna strona pobierania: <https://www.debian.org/distrib/>
- Instalacja sieciowa: <https://www.debian.org/distrib/netinst>
- Katalog bieżącego ISO `netinst` dla amd64: <https://cdimage.debian.org/debian-cd/current/amd64/iso-cd/>
- Instrukcja weryfikacji obrazów: <https://www.debian.org/CD/verify>
- Obrazy cloud, opcjonalne dla późniejszego szybkiego importu: <https://cloud.debian.org/images/cloud/>

Aktualny bezpośredni link do ISO podczas przygotowania backlogu:

<https://cdimage.debian.org/debian-cd/current/amd64/iso-cd/debian-13.6.0-amd64-netinst.iso>

To link informacyjny. Downloader powinien wykrywać nazwę aktualnego pliku w katalogu `current`, zamiast przechowywać ją na stałe.

## 4.2. Pliki instalatora do bezpośredniego bootowania

Wariant tekstowy:

- kernel: <https://deb.debian.org/debian/dists/stable/main/installer-amd64/current/images/netboot/debian-installer/amd64/linux>
- initrd: <https://deb.debian.org/debian/dists/stable/main/installer-amd64/current/images/netboot/debian-installer/amd64/initrd.gz>

Wariant graficzny GTK:

- kernel: <https://deb.debian.org/debian/dists/stable/main/installer-amd64/current/images/netboot/gtk/debian-installer/amd64/linux>
- initrd: <https://deb.debian.org/debian/dists/stable/main/installer-amd64/current/images/netboot/gtk/debian-installer/amd64/initrd.gz>

W MVP rekomendowany jest `gtk-netboot`, ponieważ testuje jednocześnie sieć, `virtio-gpu` i input. Wariant tekstowy pozostaje trybem ratunkowym i źródłem prostszych testów integracyjnych.

## 4.3. Weryfikacja autentyczności

Oficjalne obrazy Debiana mają pliki:

- `SHA256SUMS`;
- `SHA256SUMS.sign`;
- `SHA512SUMS`;
- `SHA512SUMS.sign`.

Downloader ma:

1. Pobrać obraz i odpowiedni plik sum.
2. Pobrać podpis pliku sum.
3. Zweryfikować podpis przy użyciu Debian OpenPGP keyring.
4. Dopiero wtedy zweryfikować SHA-512 obrazu.
5. Zapisać obok artefaktu manifest zawierający URL, wersję, datę pobrania i sumę.

Samo poprawne SHA-512 nie wystarcza, jeżeli plik sum nie został zweryfikowany podpisem.

## 4.4. Dwa źródła instalacji

MVP wspiera dwa sposoby:

### A. Netboot — rekomendowany

Host pobiera bezpośrednio `linux` i `initrd.gz`. Instalator pobiera pakiety z repozytoriów Debiana przez wirtualną kartę sieciową.

Zalety:

- nie trzeba emulować CD-ROM;
- małe pliki wejściowe;
- zawsze instalowane są aktualne pakiety;
- najprostsza ścieżka automatycznych testów.

### B. ISO netinst — tryb kompatybilności

Host otrzymuje ISO od użytkownika, wyciąga z niego kernel i initrd instalatora, a samo ISO wystawia jako drugi blokowy nośnik tylko do odczytu.

Docelowy układ urządzeń:

```text
/dev/vda  — pusty lub zainstalowany dysk systemowy RAW
/dev/vdb  — ISO Debiana wystawione jako read-only virtio-blk
```

Jeżeli konkretna wersja Debian Installera nie rozpozna ISO wystawionego jako zwykły `virtio-blk`, instalacja przełącza się na netboot. Pełna emulacja napędu CD/DVD nie należy do MVP.

---

# 5. Architektura robocza

```text
vmhost CLI
    │
    ├── downloader i weryfikacja Debiana
    ├── konfiguracja VM
    └── lifecycle
             │
             ▼
        VMM w Rust
             │
             ├── KVM
             ├── pamięć i vCPU
             ├── direct Linux boot
             ├── virtio-mmio
             ├── virtio-blk
             ├── virtio-net
             ├── virtio-input
             └── virtio-gpu
                      │
                      ▼
             okno winit + Vulkan/wgpu
```

Proponowana struktura repozytorium:

```text
vmhost/
├── crates/
│   ├── vmm-core/
│   ├── machine-x86/
│   ├── linux-boot/
│   ├── virtio-core/
│   ├── virtio-block/
│   ├── virtio-net/
│   ├── virtio-gpu/
│   ├── virtio-input/
│   ├── display/
│   ├── debian-media/
│   └── control-api/
├── apps/
│   └── vmhost-cli/
├── guest/
│   ├── bootstrap-kernel/
│   ├── bootstrap-initramfs/
│   └── test-rootfs/
└── tests/
    ├── boot/
    ├── installer/
    └── graphical/
```

---

# 6. Backlog

## EPIC 0 — Fundament i licencje

| ID | Zadanie | Priorytet | Estymata |
|---|---|---:|---:|
| MVP-001 | Spisać ADR określający architekturę i zakres MVP | P0 | 0,5 dnia |
| MVP-002 | Utworzyć workspace Rust z podziałem na moduły | P0 | 0,5 dnia |
| MVP-003 | Dodać automatyczny audyt licencji zależności | P0 | 1 dzień |
| MVP-004 | Przygotować CI: build, testy, Clippy i formatowanie | P0 | 1 dzień |
| MVP-005 | Dodać narzędzie diagnostyczne hosta | P0 | 1 dzień |
| MVP-006 | Wygenerować `THIRD_PARTY_LICENSES` i SBOM | P0 | 1 dzień |

Kryteria akceptacji:

- `cargo build --workspace` działa na czystym wspieranym hoście;
- audyt blokuje GPL, AGPL i LGPL w komponentach dostarczanych z hostem;
- program sprawdza `/dev/kvm`, dostępne rozszerzenia KVM i backend graficzny;
- raport licencji jest generowany w CI;
- QEMU nie jest zależnością uruchomieniową ani linkowaną biblioteką.

Proponowane biblioteki:

- `kvm-ioctls`;
- `kvm-bindings`;
- `vm-memory`;
- `linux-loader`;
- `vmm-sys-util`;
- `vm-superio`;
- `virtio-queue`;
- `winit`;
- `wgpu` albo `ash`.

## EPIC 1 — Minimalna maszyna KVM

| ID | Zadanie | Priorytet | Estymata |
|---|---|---:|---:|
| MVP-101 | Otwieranie `/dev/kvm` i walidacja wersji API | P0 | 0,5 dnia |
| MVP-102 | Utworzenie VM i pamięci gościa | P0 | 1 dzień |
| MVP-103 | Rejestracja pamięci przez `KVM_SET_USER_MEMORY_REGION` | P0 | 0,5 dnia |
| MVP-104 | Utworzenie i konfiguracja pojedynczego vCPU | P0 | 1 dzień |
| MVP-105 | Konfiguracja CPUID, rejestrów, GDT i segmentów | P0 | 1–2 dni |
| MVP-106 | In-kernel IRQ chip i PIT | P0 | 1 dzień |
| MVP-107 | Pętla `KVM_RUN` i obsługa VM exits | P0 | 1 dzień |
| MVP-108 | Kontrolowane zatrzymanie wszystkich vCPU | P0 | 1 dzień |
| MVP-109 | Smoke test wykonujący testowy kod maszynowy | P0 | 0,5 dnia |
| MVP-110 | Rozszerzenie do dwóch lub więcej vCPU | P1 | 1–2 dni |

Obsługiwane wyjścia KVM:

- `KVM_EXIT_IO`;
- `KVM_EXIT_MMIO`;
- `KVM_EXIT_HLT`;
- `KVM_EXIT_SHUTDOWN`;
- `KVM_EXIT_FAIL_ENTRY`;
- `KVM_EXIT_INTERNAL_ERROR`.

Kryteria akceptacji:

- guest wykonuje testowy kod i zapisuje znaną wartość do portu I/O;
- VMM może sto razy utworzyć i zniszczyć VM bez wycieku pamięci;
- błąd vCPU daje czytelny raport, a nie nieopisany crash procesu.

## EPIC 2 — Bezpośredni boot Linuksa

| ID | Zadanie | Priorytet | Estymata |
|---|---|---:|---:|
| MVP-201 | Ładowanie `bzImage` do pamięci gościa | P0 | 1 dzień |
| MVP-202 | Ładowanie initramfs | P0 | 0,5 dnia |
| MVP-203 | Budowanie `boot_params` i mapy E820 | P0 | 1–2 dni |
| MVP-204 | Przekazanie kernel command line | P0 | 0,5 dnia |
| MVP-205 | Emulacja portu szeregowego 16550 | P0 | 1 dzień |
| MVP-206 | Przechwytywanie konsoli `ttyS0` | P0 | 0,5 dnia |
| MVP-207 | Przygotowanie testowego initramfs | P0 | 1 dzień |
| MVP-208 | Automatyczny test bootowania do markera | P0 | 1 dzień |

Przykładowa linia poleceń kernela:

```text
console=ttyS0 earlyprintk=serial panic=1 reboot=k
```

Kryteria akceptacji:

- Linux dochodzi do procesu `init`;
- serial pokazuje kompletne logi kernela;
- guest wypisuje `VMHOST_GUEST_READY`;
- brak markera w określonym czasie kończy test błędem;
- panic kernela jest automatycznie wykrywany.

### Milestone A

Własny VMM odpala Linuksa do konsoli. Szacowany czas łączny: **7–12 dni pracy**.

## EPIC 3 — Fundament VirtIO

W MVP używany jest `virtio-mmio`, nie `virtio-pci`. Dzięki temu nie trzeba jeszcze implementować pełnej magistrali PCI, BAR-ów, capability list i MSI-X. Urządzenia są przekazywane znanemu kernelowi poprzez command line.

| ID | Zadanie | Priorytet | Estymata |
|---|---|---:|---:|
| MVP-301 | Implementacja transportu `virtio-mmio` | P0 | 2 dni |
| MVP-302 | Negocjacja feature bits | P0 | 1 dzień |
| MVP-303 | Zarządzanie statusem urządzenia i reset | P0 | 0,5 dnia |
| MVP-304 | Odczyt i walidacja descriptor chains | P0 | 2 dni |
| MVP-305 | Obsługa available/used ring | P0 | 1 dzień |
| MVP-306 | Przerwania przez `irqfd` | P0 | 1 dzień |
| MVP-307 | Powiadomienia kolejek przez `ioeventfd` | P0 | 1 dzień |
| MVP-308 | Bezpieczne operacje na pamięci gościa | P0 | 1 dzień |
| MVP-309 | Testy złośliwych i zapętlonych descriptorów | P0 | 1–2 dni |

Kryteria akceptacji:

- testowe urządzenie virtio wymienia dane z guestem;
- niepoprawny adres pamięci nie powoduje panic hosta;
- zapętlony descriptor chain zostaje odrzucony;
- reset urządzenia przywraca stan początkowy.

## EPIC 4 — Dysk `virtio-blk`

| ID | Zadanie | Priorytet | Estymata |
|---|---|---:|---:|
| MVP-401 | Urządzenie `virtio-blk` tylko do odczytu | P0 | 1–2 dni |
| MVP-402 | Obsługa `READ` i `GET_ID` | P0 | 1 dzień |
| MVP-403 | Obsługa `WRITE` | P0 | 1 dzień |
| MVP-404 | Obsługa `FLUSH` | P0 | 0,5 dnia |
| MVP-405 | Backend pliku RAW | P0 | 0,5 dnia |
| MVP-406 | Walidacja zakresu sektorów | P0 | 0,5 dnia |
| MVP-407 | Wiele urządzeń blokowych | P0 | 1 dzień |
| MVP-408 | Tryb read-only dla ISO | P0 | 0,5 dnia |
| MVP-409 | Tworzenie rzadkiego pliku RAW o zadanym rozmiarze | P0 | 0,5 dnia |

Kryteria akceptacji:

- kernel wykrywa `/dev/vda`;
- guest może zamontować system plików z obrazu RAW;
- zapis przeżywa restart VM;
- drugi obraz jest widoczny jako `/dev/vdb` i może być tylko do odczytu;
- operacja poza końcem dysku zwraca błąd.

## EPIC 5 — Sieć `virtio-net`

Sieć staje się częścią P0, ponieważ rekomendowany instalator `netboot` pobiera pakiety z repozytoriów Debiana.

| ID | Zadanie | Priorytet | Estymata |
|---|---|---:|---:|
| MVP-501 | Minimalne urządzenie `virtio-net` | P0 | 2 dni |
| MVP-502 | Kolejka TX | P0 | 1 dzień |
| MVP-503 | Kolejka RX | P0 | 1–2 dni |
| MVP-504 | Stały lub generowany adres MAC | P0 | 0,5 dnia |
| MVP-505 | Backend TAP | P0 | 1 dzień |
| MVP-506 | Skrypt konfigurujący bridge/NAT hosta | P0 | 1 dzień |
| MVP-507 | Instrukcja użycia `CAP_NET_ADMIN` | P0 | 0,5 dnia |
| MVP-508 | Test DHCP i DNS | P0 | 1 dzień |
| MVP-509 | Test pobierania pliku z `deb.debian.org` | P0 | 0,5 dnia |
| MVP-510 | Opcjonalny backend `vhost-net` | P2 | 2–3 dni |

Na początku wyłączamy zaawansowane offloady i multiqueue. Najpierw ma być poprawnie, później szybko.

Kryteria akceptacji:

- guest widzi interfejs sieciowy;
- DHCP przydziela adres;
- działa DNS;
- Debian Installer pobiera pakiety;
- uszkodzony pakiet lub descriptor nie crashuje hosta;
- zamknięcie VM zwalnia TAP.

## EPIC 6 — Downloader i obsługa mediów Debiana

| ID | Zadanie | Priorytet | Estymata |
|---|---|---:|---:|
| MVP-601 | Model `DistributionSource` i `DebianStableSource` | P0 | 1 dzień |
| MVP-602 | Pobieranie tekstowego kernela i initrd netboot | P0 | 0,5 dnia |
| MVP-603 | Pobieranie wariantu GTK | P0 | 0,5 dnia |
| MVP-604 | Pobieranie ISO z oficjalnego katalogu `current` | P0 | 1 dzień |
| MVP-605 | Wykrywanie aktualnego numeru wydania | P0 | 0,5 dnia |
| MVP-606 | Pobieranie `SHA512SUMS` i podpisu | P0 | 0,5 dnia |
| MVP-607 | Weryfikacja podpisu przez Debian keyring | P0 | 1–2 dni |
| MVP-608 | Weryfikacja SHA-512 artefaktu | P0 | 0,5 dnia |
| MVP-609 | Cache pobranych mediów | P0 | 1 dzień |
| MVP-610 | Manifest pochodzenia artefaktu | P0 | 0,5 dnia |
| MVP-611 | Ekstrakcja kernela i initrd z ISO | P1 | 1–2 dni |

Przykład:

```bash
vmhost fetch debian --channel stable --arch amd64 --variant gtk-netboot
```

Kryteria akceptacji:

- pobranie może zostać wznowione po przerwaniu;
- artefakt nie jest oznaczany jako gotowy przed weryfikacją;
- błędna suma lub podpis usuwa częściowy artefakt z aktywnego cache;
- ponowne wywołanie korzysta ze zweryfikowanego cache;
- manifest zapisuje dokładny URL i SHA-512.

## EPIC 7 — Okno i renderer hosta

| ID | Zadanie | Priorytet | Estymata |
|---|---|---:|---:|
| MVP-701 | Utworzenie okna przez `winit` | P0 | 0,5 dnia |
| MVP-702 | Inicjalizacja Vulkan/wgpu | P0 | 1 dzień |
| MVP-703 | Tekstura reprezentująca scanout gościa | P0 | 1 dzień |
| MVP-704 | Kopiowanie fragmentów obrazu do tekstury | P0 | 1 dzień |
| MVP-705 | Skalowanie obrazu i zachowanie proporcji | P0 | 0,5 dnia |
| MVP-706 | Obsługa minimalizacji i utraty surface | P0 | 1 dzień |
| MVP-707 | Screenshot bieżącego scanoutu | P0 | 0,5 dnia |
| MVP-708 | Licznik FPS i statystyki kopiowania | P1 | 0,5 dnia |

## EPIC 8 — `virtio-gpu` 2D

| ID | Zadanie | Priorytet | Estymata |
|---|---|---:|---:|
| MVP-801 | Szkielet urządzenia i kolejki `controlq`/`cursorq` | P0 | 1 dzień |
| MVP-802 | `GET_DISPLAY_INFO` | P0 | 0,5 dnia |
| MVP-803 | `RESOURCE_CREATE_2D` | P0 | 1 dzień |
| MVP-804 | `RESOURCE_ATTACH_BACKING` | P0 | 1 dzień |
| MVP-805 | `TRANSFER_TO_HOST_2D` | P0 | 1–2 dni |
| MVP-806 | `SET_SCANOUT` | P0 | 1 dzień |
| MVP-807 | `RESOURCE_FLUSH` | P0 | 1 dzień |
| MVP-808 | `RESOURCE_UNREF` i odpinanie pamięci | P0 | 1 dzień |
| MVP-809 | Format `B8G8R8A8_UNORM` | P0 | 0,5 dnia |
| MVP-810 | Dirty rectangles | P0 | 1 dzień |
| MVP-811 | EDID i lista trybów | P1 | 1–2 dni |
| MVP-812 | Sprzętowy kursor | P1 | 1 dzień |
| MVP-813 | Zmiana rozdzielczości po resize | P1 | 1–2 dni |

Minimalny przepływ:

```text
Guest tworzy zasób 2D
        ↓
Podpina strony pamięci
        ↓
Ustawia zasób jako scanout
        ↓
TRANSFER_TO_HOST_2D
        ↓
RESOURCE_FLUSH
        ↓
Host aktualizuje teksturę Vulkan
        ↓
Obraz pojawia się w oknie
```

Kryteria akceptacji:

- kernel ładuje `virtio_gpu`;
- `/dev/dri/card0` istnieje;
- instalator i późniejszy Weston są widoczne w oknie;
- działa minimum 1920×1080;
- VM nie może wymusić kopiowania spoza swojej pamięci;
- pełne odświeżenie ekranu nie pozostawia artefaktów.

## EPIC 9 — `virtio-input`

| ID | Zadanie | Priorytet | Estymata |
|---|---|---:|---:|
| MVP-901 | Klawiatura `virtio-input` | P0 | 1–2 dni |
| MVP-902 | Mapowanie keycode host → Linux input event | P0 | 1 dzień |
| MVP-903 | Absolutne urządzenie wskazujące | P0 | 1 dzień |
| MVP-904 | Przyciski i scroll myszy | P0 | 0,5 dnia |
| MVP-905 | Synchronizacja `EV_SYN` | P0 | 0,5 dnia |
| MVP-906 | Focus i zwalnianie kursora | P0 | 0,5 dnia |
| MVP-907 | Skrót awaryjnego zwalniania inputu | P0 | 0,5 dnia |
| MVP-908 | Test inputu przez `evtest` | P0 | 0,5 dnia |

Proponowane skróty:

```text
Ctrl + Alt + G — przechwyć lub zwolnij input
Ctrl + Alt + Q — poproś o zamknięcie VM
```

Kryteria akceptacji:

- można przejść cały instalator klawiaturą i myszą;
- absolutna pozycja kursora zgadza się z oknem;
- utrata focusu zwalnia wszystkie wciśnięte klawisze;
- guest nie może przejąć globalnego inputu hosta.

## EPIC 10 — Tryb instalatora

| ID | Zadanie | Priorytet | Estymata |
|---|---|---:|---:|
| MVP-1001 | Komenda `vmhost disk create` | P0 | 0,5 dnia |
| MVP-1002 | Komenda `vmhost install` | P0 | 1 dzień |
| MVP-1003 | Profil parametrów kernela Debian Installera | P0 | 1 dzień |
| MVP-1004 | Boot tekstowego netboot installera | P0 | 1 dzień |
| MVP-1005 | Boot graficznego instalatora GTK | P0 | 1–2 dni |
| MVP-1006 | Podpięcie docelowego RAW jako `/dev/vda` | P0 | 0,5 dnia |
| MVP-1007 | Opcjonalne podpięcie ISO jako `/dev/vdb` read-only | P1 | 0,5 dnia |
| MVP-1008 | Wykrywanie końca instalacji i rebootu | P0 | 1 dzień |
| MVP-1009 | Zapis profilu zainstalowanej VM | P0 | 1 dzień |
| MVP-1010 | Automatyczny test instalacji preseed | P0 | 2–3 dni |

Przykład:

```bash
vmhost disk create debian.raw --size 32G
vmhost install debian --disk debian.raw --variant gtk-netboot
```

Kryteria akceptacji:

- instalator uruchamia się bez BIOS-u i UEFI;
- widzi sieć, dysk i urządzenia wejściowe;
- może utworzyć partycje na `/dev/vda`;
- może pobrać i zainstalować system;
- automatyczny profil testowy kończy instalację bez interakcji;
- ręczna instalacja pozostaje możliwa.

## EPIC 11 — Boot zainstalowanego systemu bez UEFI

W pierwszej wersji host nadal uruchamia kernel bezpośrednio. Zainstalowany system znajduje się na `debian.raw`, ale potrzebuje kernela startowego dostarczonego przez host.

Najprostszy wariant P0:

1. Host uruchamia utrzymywany przez projekt kernel bootstrapowy.
2. Kernel ma wbudowane sterowniki virtio-blk, virtio-net, virtio-gpu i virtio-input.
3. Bootstrap initramfs montuje root z `/dev/vda1` lub na podstawie wskazanego UUID.
4. System przechodzi do `switch_root` i uruchamia `/sbin/init` z zainstalowanego Debiana.

W tym wariancie aktualizacja kernela wewnątrz Debiana nie zmienia kernela startowego. To jawne ograniczenie MVP.

| ID | Zadanie | Priorytet | Estymata |
|---|---|---:|---:|
| MVP-1101 | Konfiguracja bootstrapowego kernela | P0 | 1 dzień |
| MVP-1102 | Reprodukowalny build kernela i initramfs | P0 | 1–2 dni |
| MVP-1103 | Wykrywanie partycji root | P0 | 1 dzień |
| MVP-1104 | Montowanie root i `switch_root` | P0 | 1 dzień |
| MVP-1105 | Obsługa UUID rootfs w konfiguracji | P0 | 0,5 dnia |
| MVP-1106 | Diagnostyka błędnego lub brakującego rootfs | P0 | 0,5 dnia |
| MVP-1107 | Eksperymentalny bootstrap przez `kexec` do kernela z `/boot` | P2 | 3–5 dni |

Kryteria akceptacji:

- świeżo zainstalowany Debian bootuje bez ręcznego kopiowania kernela;
- bootstrap wyświetla czytelny błąd, gdy nie znajduje rootfs;
- moduły niezbędne przed `switch_root` są wbudowane w kernel lub initramfs;
- ograniczenie aktualizacji kernela jest opisane użytkownikowi.

Docelowo cały ten mechanizm zastąpi boot przez UEFI/OVMF.

## EPIC 12 — Konfiguracja i lifecycle

| ID | Zadanie | Priorytet | Estymata |
|---|---|---:|---:|
| MVP-1201 | Format konfiguracji TOML | P0 | 1 dzień |
| MVP-1202 | Komenda `run` | P0 | 0,5 dnia |
| MVP-1203 | Stany `Created/Running/Stopping/Stopped/Crashed` | P0 | 1 dzień |
| MVP-1204 | Obsługa `SIGINT` i `SIGTERM` | P0 | 0,5 dnia |
| MVP-1205 | Czytelne komunikaty diagnostyczne | P0 | 1 dzień |
| MVP-1206 | Logi strukturalne z identyfikatorem VM | P0 | 0,5 dnia |
| MVP-1207 | Graceful shutdown przez kanał host–guest | P1 | 1–2 dni |

Przykładowa konfiguracja:

```toml
name = "debian-demo"
memory_mib = 2048
vcpus = 2

[boot]
mode = "direct-linux"
kernel = "artifacts/bootstrap/vmlinuz"
initramfs = "artifacts/bootstrap/initrd.img"
cmdline = "console=ttyS0 root=/dev/vda1 rw"

[[disk]]
path = "images/debian.raw"
writable = true

[network]
backend = "tap"
interface = "vmhost0"

[display]
width = 1920
height = 1080
scale = 1.0
```

## EPIC 13 — Testowy desktop Debiana

| ID | Zadanie | Priorytet | Estymata |
|---|---|---:|---:|
| MVP-1301 | Profil instalacji minimalnego Debiana | P0 | 1 dzień |
| MVP-1302 | Instalacja Mesa i Westona | P0 | 0,5 dnia |
| MVP-1303 | Automatyczne logowanie użytkownika testowego | P0 | 0,5 dnia |
| MVP-1304 | Automatyczny start Westona | P0 | 0,5 dnia |
| MVP-1305 | Program testujący animację, resize i input | P0 | 1 dzień |
| MVP-1306 | Wyświetlanie wersji kernela i renderera Mesa | P0 | 0,5 dnia |

Kryteria akceptacji:

- po bootowaniu automatycznie pojawia się pulpit;
- animacja pozwala wykryć problemy z odświeżaniem;
- test pokazuje aktualną rozdzielczość i renderer Mesa;
- screenshot może być automatycznie porównany z wzorcem.

## EPIC 14 — Bezpieczeństwo, testy i pakowanie

| ID | Zadanie | Priorytet | Estymata |
|---|---|---:|---:|
| MVP-1401 | Unit testy pamięci i virtqueue | P0 | 1–2 dni |
| MVP-1402 | Fuzzing parsera descriptor chains | P0 | 1–2 dni |
| MVP-1403 | Test 100 kolejnych bootów | P0 | 1 dzień |
| MVP-1404 | Test działania VM przez osiem godzin | P0 | 1 dzień |
| MVP-1405 | Automatyczny test rozdzielczości | P0 | 0,5 dnia |
| MVP-1406 | Porównywanie screenshotu z wzorcem | P0 | 1 dzień |
| MVP-1407 | Ograniczenia zasobów i descriptorów | P0 | 1 dzień |
| MVP-1408 | Test pełnej instalacji Debiana | P0 | 1–2 dni |
| MVP-1409 | Dokumentacja uruchomienia | P0 | 1 dzień |
| MVP-1410 | Paczka binarna dla jednego host OS | P0 | 1 dzień |

Finalne testy akceptacyjne:

- 100/100 poprawnych bootów przygotowanego obrazu;
- kompletna instalacja Debiana w trybie automatycznym;
- osiem godzin animacji bez istotnego wycieku pamięci;
- 1920×1080 i 2560×1440;
- działają klawiatura, mysz, sieć i resize;
- uszkodzony descriptor nie crashuje hosta;
- przerwanie VMM-u nie uszkadza ukończonego obrazu dysku;
- host pozostaje responsywny podczas pracy VM.

---

# 7. Rozszerzenie MVP — akcelerowane OpenGL

Rutabaga udostępnia przenośną warstwę wirtualizacji grafiki i backendy takie jak virglrenderer oraz gfxstream. W pierwszym rozszerzeniu wybieramy klasyczny VirGL/OpenGL, nie Venus/Vulkan.

Projekt: <https://github.com/magma-gpu/rutabaga_gfx>

| ID | Zadanie | Priorytet | Estymata |
|---|---|---:|---:|
| GPU-001 | Spike `rutabaga_gfx + virglrenderer` | P1 | 2–3 dni |
| GPU-002 | Negocjacja `VIRTIO_GPU_F_VIRGL` | P1 | 1 dzień |
| GPU-003 | Pobieranie capsetów VirGL | P1 | 1 dzień |
| GPU-004 | Tworzenie i niszczenie kontekstów 3D | P1 | 2 dni |
| GPU-005 | `RESOURCE_CREATE_3D` | P1 | 1–2 dni |
| GPU-006 | Podpinanie zasobów do kontekstu | P1 | 1 dzień |
| GPU-007 | `SUBMIT_3D` | P1 | 2 dni |
| GPU-008 | Transfery 3D | P1 | 1–2 dni |
| GPU-009 | Fence'y i synchronizacja | P1 | 3–5 dni |
| GPU-010 | Integracja renderowanego zasobu ze scanoutem | P1 | 2 dni |
| GPU-011 | Test `glmark2-es2-wayland` | P1 | 1 dzień |
| GPU-012 | Recovery po błędzie kontekstu GPU | P1 | 2–4 dni |

Definition of Done dla 3D:

- `glxinfo -B` albo `eglinfo` pokazuje renderer VirGL zamiast `llvmpipe`;
- `glmark2-es2-wayland` działa minimum godzinę;
- host nadal korzysta z tego samego fizycznego GPU;
- zamknięcie VM zwalnia konteksty i zasoby;
- błąd renderera nie zabija pulpitu hosta.

Aktualnego `rust-vmm/vhost-device-gpu` nie należy traktować jako kompletnego gotowca do całego 3D. Nadal występują ograniczenia dotyczące blob resources, Venus i sprzętowej akceleracji części backendów: <https://github.com/rust-vmm/vhost-device/blob/main/vhost-device-gpu/README.md>.

---

# 8. Harmonogram

| Milestone | Efekt | Czas łączny |
|---|---|---:|
| A | Linux w konsoli | 1–2 tygodnie |
| B | Linux z dysku `virtio-blk` | 2–3 tygodnie |
| C | Sieć i pobieranie Debiana | 3–4 tygodnie |
| D | Instalator Debiana w oknie | 4–7 tygodni |
| E | Zainstalowany system, input i stabilizacja | **6–10 tygodni** |
| F | Akcelerowane OpenGL/VirGL | **9–14 tygodni** |

Pierwszy efekt „własny host odpala Linuksa” powinien pojawić się po około dwóch tygodniach. Pierwszy instalator widoczny w oknie jest realistyczny około czwartego–szóstego tygodnia. Używalne MVP z instalacją, siecią, 2D i inputem wymaga około **35–55 dni inżynierskich**.

---

# 8a. Backlog post-MVP (dopisane 2026-08-18 po dowiezieniu MVP)

## EPIC 15 — Okno VM: kursor i skalowanie

| ID | Zadanie | Priorytet |
|---|---|---:|
| WIN-1501 | Ukrywanie kursora hosta nad ekranem gościa przy aktywnym grabie | P0 |
| WIN-1502 | Ctrl+Alt zwraca kursor (zwalnia grab); klik w obraz gościa przywraca grab | P0 |
| WIN-1503 | Swobodne ręczne skalowanie okna (resize z zachowaniem letterboxa) | P0 |
| WIN-1504 | Pełny ekran (F11) i tryb 1:1 | P1 |

## EPIC 16 — Menedżer GUI (natywny, bez Electrona)

Elegancki, nowoczesny, lekko futurystyczny UI nawiązujący do mechaniki
kwantowej (motyw splątania). Stack: egui/eframe na wgpu — natywnie, wydajnie.

| ID | Zadanie | Priorytet |
|---|---|---:|
| GUI-1601 | Aplikacja `entangled-manager`: lista maszyn (profil + status) | P0 |
| GUI-1602 | Kreator nowej maszyny (nazwa/RAM/vCPU/dysk) z automatyczną instalacją | P0 |
| GUI-1603 | Start/Stop maszyny (proces potomny `entangled run`) | P0 |
| GUI-1604 | Usuwanie maszyny (dysk + profil) z potwierdzeniem | P0 |
| GUI-1605 | Podgląd logu instalacji/konsoli w UI | P1 |
| GUI-1606 | Motyw "quantum": ciemny, akcenty cyan/fiolet, subtelne animacje | P0 |

## EPIC 17 — Natywny host Windows (WHP)

Zgodnie z ADR-0002: drugi backend hypervisora za traitem.

| ID | Zadanie | Priorytet |
|---|---|---:|
| WHP-1701 | Trait `Hypervisor/Vm/Vcpu` w vmm-core; backend KVM za nim | P0 |
| WHP-1702 | Backend WHP: partycja, pamięć, vCPU, pętla run, exity IO/MMIO | P0 |
| WHP-1703 | Userspace PIC/IOAPIC/PIT (WHP daje tylko lokalny APIC) | P0 |
| WHP-1704 | Sieć user-mode (smoltcp NAT) — bez TAP, bez GPL | P0 |
| WHP-1705 | Budowa i testy na Windows (toolchain gnu, CI matrix) | P0 |

## EPIC 18 — UEFI i akceleracja GPU

| ID | Zadanie | Priorytet |
|---|---|---:|
| UEFI-1801 | Urządzenie pflash + mapowanie firmware w pamięci gościa | P0 |
| UEFI-1802 | Firmware EDK2 (wariant CloudHv-style, virtio-mmio) — build + boot | P0 |
| UEFI-1803 | Boot ISO Ubuntu przez UEFI (virtio-blk read-only jako nośnik) | P0 |
| UEFI-1804 | Instalacja i boot Ubuntu end-to-end | P0 |
| GPU-18xx | VirGL/Rutabaga wg istniejącej sekcji 7 (GPU-001..012) | P1 |

# 9. Następna faza po MVP

Najbardziej logiczny kolejny etap:

1. `virtio-pci` i minimalna magistrala PCI;
2. UEFI przez dokładnie zaudytowane OVMF albo permissive firmware;
3. klasyczny boot z dowolnego ISO;
4. bootloader i kernel przechowywane wyłącznie wewnątrz dysku VM;
5. panel Avalonia/.NET;
6. audio i shared folders;
7. snapshoty;
8. dopiero potem eksperymenty z Windowsowym guestem.

