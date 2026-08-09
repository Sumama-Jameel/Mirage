# Rule 1

- THIS FILE IS MY EVERYTHING - Goal, requirements, plan, aimbition, constraints, etc. ANYTHING WRITTEN ON THIS FILE IS CONCRETE AND HIGH PRIORITY AND SHOULD NOT BE VIOLATED.
- FOCUS ON MY WORDS, NO MATTER IF YOU ARE READING THIS FILE FOR THE FIRST TIME OR FOR THE 5000000000000000000 TIME. USE INTENTPRETER IF NEEDED. BUT FOR SMALL SECTIONS, NOT FOR ENTIRE FILE.

# Goal

- My goal is to use the internal API of AI providers in browsers, and exposed them to user via openAI compatible API. And provide every single necessary features that openAI API provide(no those temprature, little fields), but major ones like NATIVE tool calling with XML output as fallback, streaming, file upload, ETC. YOU WILL BE USING WEB SEARCH TOOLS TO GET SOME RELATED OR SIMILAR OPENSOURCE PROJECTS. Obscura browser can be used for some helps. 
As many AI models in browser are free. No heavy GPU or paid API.
ALWAYS PREFER THE ACTUAL BACKGROUND API, NOT THE CHAT AUTOMATION. ALWAYS REVERSE ENGINEER INTERNAL API OF WHAT THEIR WEB USES, AND USE THAT. CHAT AUTOMATION EXISTS, BUT WILL BE REMOVED LATER.


- Only use FALLBACKS LIKE XML FOR TOOL CALLING, when the feature/field REALLY DOESN'T EXIST AT THE ROOT LEVEL. But this is risky, so
You must have strong, latest evidence that it doesn't exist. YOU MUST BROWSE HERE TO CONFIRM IF IT EXIST OR NOT, DONT DO ANYTHING
BASED ON GUESS, OR UNVERIFIED ASSUMPTIONS.


- And remember: TAKE THIS PROJECT AS PERSONAL, TRY TO PROVIDE EVERY SINGLE MAJOR LLM AND FEATURE WE CAN PROVIDE. UNDERSTAND THE GOAL, THE AIMBITION AND TAKE DECISION INDEPENDENTLY. NOT BASED ON COMPLEXITY OR  TIME CONSTRAINTS, OR ANYTHING LIKE THAT.

- READ WillAddLater for future models and this file("GOAL.md") for goal understanding. All model are added.
- READ LatestAImodels to get the info of latest model available on the web of their provider, it is updated by me, dont do any changes. But we MUST support every provider listed there.
- To run anything on background properly, use proper tools like setsid or nohup. Otherwise terminal timeout will kill it as the terminal stop.
- /home/sumama/Private/Mirage/obscura/BROKEN_FEATURES.md This file containts the latest bugs and broken stuffs found, also consider them, some of em might be fixed but most of them are not. BUT STILL, DONT STOP EVEN WHEN BROKEN STUFFS IN THIS FILE ARE FIX. CONTINUE AND MEET "When to stop" section and it requirements below, then you can stop.
- The gateway have massive things that aren't comitted, dont waste time viewing git diff. It is uncomitted intentionally, AND NEVER EVER EVEN CREATE A REPO OR COMMIT OR PUSH UNLESS I SAY.

# GLOBAL MUST RULES:

- NOTHING SHOULD BE DONE BASED ON LIKELY, MAYBE, UNVERIFIED ASSUMPTIONS, PROBABLY, STALE OR OUTDATED DATA/INFO.
- FROM some previous sessions, you just sometimes keep running tests, cargo, etc. And just stuck in a loop of testing or researching. DONT TO THAT, NEVER EVER DOWNLOAD NEXEST, I KNOW AGENTS.MD SAYS THAT, BUT DONT DO THAT.
- NEVER EVER HARDCODE ANY TYPE OF FIX.
- EVERY SINGLE THING MUST BE DONE, INTEGRATED AND WORKING PROPERLY, here i am not talking about "files written", syntax check, tests, build or release. I am talking about live test, of EVERY SINGLE PROVIDER, WITH EVERY SINGLE FEATURE, JUST DONT SPAM OTHERWISE YOU WILL GET RATE LIMITED.
- For each provider, we must support:

- NATIVE tool calling
- NATIVE streaming and non streaming
- NATIVE file upload
- NATIVE Thinking toggle
- NATIVE research/deep-research toggle
- Session continuation/resuming

- No shortcuts.
- This project itself is a research heavy project, YOU MUST USE RESEARCH TOOLS TO SEARCH THE INTERNET FOR LATEST INFO AND OTHER REVERSE ENGINEERED PROJECTS FOR HINTS OR HELPS.
- EVERYTHING SHOULD BE BLAZING FAST IN THIS PROJECT, JUST LIKE IT IS RIGHT NOW.
- NO HEAVY DEPS OR PAID RESOURCE SHOULD BE USED.
- Do not consider this project as an MVP, its a real product from the start.
- Don't provide me a summary or ask me until you meet "When to stop" section requirements.
- You should not escape from COMPLEX WORK.
- Progress should be real , not just "planning from previous 5 hours".
- Every time you research and find something useful or very useful, that is kind of "one time" information, save it inside a markdown file in docs folder, so you dont repeat researches later.
- Dont waste my tokens by doing useless things, that doesn't have any benefits. OR useful outputs. I have limited access to you.

# When to stop

- For you stop working, you have to meet ALL OF THESE REQUIREMENTS:

- You have implemented every single provider in this project.
- All test passes and build complete. WITH NO WARNINGS. 
- Every single model is working.
- ALL TYPE OF COMPONENTS LIKE TOOL CALLING, WEB SEARCH, FILE UPLOAD, STREAMING, NON STREAMING, ETC - IS WORKING 100% PROPERLY WITH NATIVE PREFERRED.
- DO NOT STOP UNTIL ALL OF THIS IS DONE PROPERLY.