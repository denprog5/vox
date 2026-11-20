# Промпты для Codex (встроенный генератор картинок)

## Базовый кадр (source/cat-idle.png), с фото-референсом

```
codex exec -s workspace-write --skip-git-repo-check --image source/ref-cat-photo.png -- '<промпт>'
```

> Generate an image with your built-in image generation tool (the subscription based image_gen tool of this session). Do NOT use the $imagegen CLI script, do NOT use OPENAI_API_KEY or any API calls, do NOT draw the image with code, do NOT create placeholders. Use the attached photo only as a reference for the cat appearance (fur color, face shape, eye color, chest patch). Image: a stylized cartoon character, NOT photorealistic. Soft painterly cartoon illustration, hand painted digital gouache look like a cozy indie game or childrens picture book, soft warm lighting, visible fluffy fur brush strokes, clean readable silhouette, gentle dark outline. Character: a chubby blue gray British Shorthair cat with a very round face, full round cheeks, small rounded ears, big round amber yellow green eyes, a small cream white patch on the chest, faint darker tabby stripes on the forehead. Pose: front view bust portrait, the cat sits upright like a streamer at a desk, both chubby front paws resting on the bottom edge of the frame, looking straight at the viewer, calm friendly neutral expression, mouth closed. Centered, the whole head and ears fully inside the frame with empty margin around the ears, the body is cut by the bottom edge. Background: fully transparent background (PNG with alpha). No text, no logo, no accessories, no props. Size 1024x1024. Save the final PNG as cat-idle.png in the current directory.

## Эмоции: правка базового кадра

```
codex exec -s workspace-write --skip-git-repo-check --image source/cat-idle.png -- '<шаблон>'
```

Шаблон:

> Edit the attached image with your built-in image generation tool (the subscription based image_gen tool of this session). Do NOT use the $imagegen CLI script, do NOT use OPENAI_API_KEY or any API calls, do NOT edit it with code. This is one frame of a set of expression variants of the same cartoon character, so it must line up perfectly with the original: keep exactly the same character, same painterly gouache style, same colors, same body, same paws, same pose, same size, same position and framing in the canvas, same fully transparent background (PNG with alpha). Change ONLY the facial expression: **<ЭМОЦИЯ>**. Keep everything else unchanged. Output 1024x1024. Save the result as **<имя>**.png in the current directory.

| Файл | ЭМОЦИЯ |
|---|---|
| cat-blink | the eyes are gently closed, drawn as calm curved closed eyelid lines, relaxed face, mouth closed |
| cat-talk | the cat is talking: mouth open in a medium sized rounded open mouth showing a little pink tongue, eyes open and friendly, slightly lifted eyebrows |
| cat-shout | the cat is shouting loudly: mouth opened extremely wide in a big yell showing small fangs and tongue, eyes wide open with tiny pupils, eyebrows angry, ears pushed slightly back, fur a bit puffed up |
| cat-sad | the cat is very sad and upset: ears drooping down to the sides, eyebrows tilted up in the middle, big glossy watery eyes with tears welling up and one tear rolling down a cheek, small downturned frowning mouth, mouth closed |
| cat-sad-talk | same as cat-sad but mouth slightly open in a wobbly sad frown showing a little pink tongue |
| cat-laugh | the cat is laughing out loud very happily: eyes squeezed shut as happy upward arcs, a small tear of joy at the corner of one eye, cheeks raised, mouth wide open in a big joyful laugh showing pink tongue and tiny fangs, ears perked up |

После генерации: `python tools/clean.py`.

## Добавлено позже (через `bash tools/gen.sh <имя> <база> "<изменение>"`)

| Файл | База | Изменение |
|---|---|---|
| cat-surprise | cat-idle | ONLY the facial expression changes: the cat is very surprised and amazed: eyes opened extremely wide and round with small pupils and big shiny highlights, eyebrows raised high, ears straight up, mouth a small round open O shape |
| cat-sing | cat-idle | the cat is singing happily: eyes closed in a blissful way, head slightly tilted to one side, mouth open in a rounded O shape like singing a long note, two small painted musical notes floating in the empty space beside the head |
| cat-eat-1 | cat-idle | the cat is happily eating: a small round cream colored ceramic plate with a piece of salmon fish and a few kibble pieces stands at the bottom edge of the frame right in front of the cat between its two front paws. The cat head is tilted down toward the plate, eyes happily closed as content arcs, cheeks puffed full, mouth closed chewing, a tiny crumb on the chin |
| cat-eat-2 | cat-eat-1 | ONLY the mouth changes: the cat is chewing with the mouth slightly open showing a tiny bit of pink tongue and a small piece of salmon, cheeks still puffed, eyes still happily closed. The plate, food, paws and everything else stay exactly the same |
