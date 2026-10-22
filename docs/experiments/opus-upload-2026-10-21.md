# Envio em Opus: tamanho, tempo e precisão

Data: 21 de outubro de 2026. Status: adotado para os envios de clipes gravados.

**Decisão: enviar Opus a 24 kbit/s aos provedores que aceitam Ogg Opus.** O
arquivo cai para cerca de 10% do WAV. No conjunto de teste, com o Whisper como
modelo de referência, nenhuma variante de Opus de 12 a 32 kbit/s mudou o WER
de forma mensurável. As seções abaixo têm papéis diferentes: a do Whisper é o
estudo de precisão; a do teste pago é a verificação de aceitação, com sete
provedores reais.

## Protocolo

- Corpus público [Google FLEURS](https://huggingface.co/datasets/google/fleurs),
  `pt_br`, divisão `validation`: as 80 primeiras frases distintas, com 1.771
  palavras de referência por condição.
- Duas condições: o áudio original e o mesmo áudio com ruído rosa fixo
  (`anoisesrc`, amplitude 0,02, semente 7).
- Cada variante foi codificada, decodificada de volta para PCM a 16 kHz e
  transcrita pelo Whisper large-v3-turbo (q5_0, whisper.cpp 1.9.5, idioma `pt`).
  O Whisper serve de modelo de referência local; os provedores do Endu não
  foram chamados.
- O WER ignora pontuação e maiúsculas. O intervalo é um bootstrap pareado de
  95% sobre a diferença em relação ao WAV. Os intervalos têm de 0,9 a 1,8
  ponto de largura: com 80 frases, só diferenças a partir de cerca de 0,8
  ponto seriam detectáveis.

## Resultados

| Variante | Tamanho vs WAV | Δ WER (limpo) | Δ WER (ruído) |
|---|---|---|---|
| FLAC | ~60% | 0,00 | 0,00 |
| libopus 32k | 12% | +0,06 [−0,33; +0,42] | −0,51 [−1,39; +0,05] |
| libopus 24k | 9% | +0,06 [−0,33; +0,41] | −0,40 [−1,24; +0,24] |
| libopus 16k | 6% | +0,28 [−0,28; +0,85] | −0,45 [−1,31; +0,12] |
| libopus 12k | 5% | +0,06 [−0,50; +0,58] | −0,40 [−1,36; +0,40] |
| libopus 8k | 3% | +0,51 [−0,23; +1,43] | **+0,79 [+0,23; +1,38]** |
| AAC 32k | 13% | −0,06 [−0,52; +0,35] | −0,45 [−1,32; +0,23] |
| MP3 32k | 13% | −0,28 [−0,91; +0,29] | −0,51 [−1,42; +0,11] |
| Endu, 24k, qualidade padrão | 9% | +0,23 [−0,22; +0,68] | −0,28 [−1,19; +0,40] |
| Endu, 24k, qualidade mínima | 9% | +0,17 [−0,55; +0,92] | −0,45 [−1,34; +0,29] |

O WER do WAV foi 3,73% no áudio limpo e 4,29% com ruído. As linhas "Endu" foram
medidas antes de corrigir o granule das páginas intermediárias; o erro só
cortava os 6,5 ms finais de cada clipe na decodificação. Só o Opus a 8 kbit/s
piorou de forma significativa, coerente com a literatura: ele muda para banda
estreita nessa taxa (NoLACE, ICASSP 2024; Amazon, arXiv 2002.00122).

## Tempo

- Rede medida (Wi-Fi, 50–110 Mbit/s de subida): um clipe de 30 s em base64
  ocupa 1,28 MB e sobe em 0,12–0,21 s. A 5 Mbit/s seriam ~2 s; em Opus, ~0,2 s.
- O codificador do AudioToolbox leva ~230 ms por 30 s de áudio na qualidade
  padrão e ~63 ms na mínima, com o mesmo tamanho (Apple M2). O Endu usa a
  mínima, porque a codificação fica entre o Finish e o envio.
- Handshake TLS de uma conexão nova: 20–80 ms na maioria dos provedores e
  ~365 ms no Deepgram.

## Reuso de conexão (22 de outubro)

O envio passou a usar um agente `ureq` compartilhado. Quando o modelo principal
não transcreve ao vivo, um HEAD sem credenciais abre a conexão quando a
gravação passa do limiar de toque intencional. Mediana de 5 POSTs de 92 KB sem chave (resposta 401), com 3 s de
espera entre o aquecimento e o envio:

| Provedor | Conexão nova | Pré-aquecida | Ganho |
|---|---|---|---|
| OpenRouter | 73 ms | 42 ms | −31 ms |
| xAI (Grok) | 293 ms | 276 ms | −17 ms |
| ElevenLabs | 239 ms | 160 ms | −79 ms |
| Deepgram | 569 ms | 254 ms | −315 ms |
| OpenAI | 263 ms | 229 ms | −34 ms |
| Google | 504 ms | 425 ms | −79 ms |

Com 35 s de espera (uma amostra por provedor), OpenRouter, ElevenLabs, OpenAI
e Google ainda reaproveitaram a conexão; Grok e Deepgram ficaram iguais a uma
conexão nova. Medições depois disso mostraram o tempo até o servidor fechar
uma conexão ociosa: Deepgram ~5 s, Meta ~70 s, xAI ~200 s, os demais mais. A
amostra única de 35 s do Grok foi ruído. O limite de ociosidade do pool fica
no código (`src/providers/http_pool.rs`), abaixo do menor desses tempos que
vale manter.

O Deepgram não é pré-aquecido, apesar de ter o maior ganho medido: ele fecha
a conexão ociosa em ~5 s, então o socket aberto durante a fala quase sempre
já caiu no Finish, e o aquecimento só somaria uma requisição por ditado.

## Provedores (teste pago, 22 de outubro)

Seis frases do mesmo conjunto (141 palavras), enviadas em WAV e no Ogg Opus
gerado pelo código do Endu, com idioma `pt`. Status 200 por envio, WER e
mediana da latência da requisição:

| Provedor e modelo | Opus aceito | WER WAV → Opus | Mediana WAV → Opus |
|---|---|---|---|
| Deepgram nova-3 | 6/6 | 7,09 → 7,09% | 1278 → 871 ms |
| ElevenLabs scribe_v2 | 5/6 (um 429 de sobrecarga) | 1,42 → 0,83% | 2404 → 2366 ms |
| xAI grok-voice-transcribe-2.0 | 6/6 | 1,42 → 2,13% | 681 → 607 ms |
| Google gemini-3.5-transcribe | 6/6 | 3,20 → 1,42% | 1760 → 1237 ms |
| OpenAI gpt-transcribe | 6/6 | 1,42 → 1,42% | 1826 → 1094 ms |
| OpenAI gpt-4o-mini-transcribe | 6/6 | 1,42 → 2,13% | 1469 → 912 ms |
| OpenRouter microsoft/mai-transcribe-2 (`format: "ogg"`) | 6/6 | 1,42 → 1,42% | 1721 → 1573 ms |
| Meta muse-voice-transcribe-1.0 | 0/6 | 5,67% → recusado | — |

Uma palavra em 141 vale 0,71 ponto: as diferenças de WER estão no ruído. A
Meta recusou o Opus com `audioEncoding` `WAV` ("must be a mono 16-bit PCM
WAV"), `OGG_OPUS` e `OPUS` ("Malformed request"); sua documentação só permite
`WAV`. A OpenAI aceita Ogg apesar de o guia não listar o formato. No
OpenRouter só a rota MAI foi testada; as outras continuam em WAV.

Regras de fallback no app: um 415, ou um 400/422 cuja resposta nomeia o
formato do áudio, repete a mesma tentativa uma vez em WAV, e a recusa fica
lembrada para o modelo até o app ser reaberto. Um 2xx sem texto também é
repetido uma vez em WAV, mas só desliga o Opus para o modelo se o WAV trouxer
texto; depois que o modelo já transcreveu um Opus, respostas vazias são
aceitas sem reenvio.
